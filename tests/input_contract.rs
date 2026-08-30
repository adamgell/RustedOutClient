use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use rustedoutclient::{
    connection::{bounded_vnc_channels, VNC_QUEUE_CAPACITY},
    session::InputAction,
    vnc::{ClipboardText, InputController, InputError, InputSink},
};

const CONTROL_L: u32 = 0xFFE3;
const ALT_L: u32 = 0xFFE9;
const DELETE: u32 = 0xFFFF;
const CLIPBOARD_LIMIT: usize = 1_048_576;

#[derive(Clone, Debug, Eq, PartialEq)]
enum Attempt {
    Key { down: bool, keysym: u32 },
    Pointer { buttons: u8, x: u16, y: u16 },
    Clipboard(usize),
}

#[derive(Default)]
struct SinkState {
    attempts: Vec<Attempt>,
    failures: BTreeMap<usize, InputError>,
}

#[derive(Clone, Default)]
struct RecordingSink(Arc<Mutex<SinkState>>);

impl RecordingSink {
    fn fail_with(self, failures: impl IntoIterator<Item = (usize, InputError)>) -> Self {
        self.0.lock().unwrap().failures.extend(failures);
        self
    }

    fn attempts(&self) -> Vec<Attempt> {
        self.0.lock().unwrap().attempts.clone()
    }

    fn record(&self, attempt: Attempt) -> Result<(), InputError> {
        let mut state = self.0.lock().unwrap();
        state.attempts.push(attempt);
        let attempt_number = state.attempts.len();
        state
            .failures
            .get(&attempt_number)
            .copied()
            .map_or(Ok(()), Err)
    }
}

impl InputSink for RecordingSink {
    fn key(&mut self, down: bool, keysym: u32) -> Result<(), InputError> {
        self.record(Attempt::Key { down, keysym })
    }

    fn pointer(&mut self, buttons: u8, x: u16, y: u16) -> Result<(), InputError> {
        self.record(Attempt::Pointer { buttons, x, y })
    }

    fn send_clipboard(&mut self, text: String) -> Result<(), InputError> {
        self.record(Attempt::Clipboard(text.len()))
    }
}

fn ready_controller(
    sink: RecordingSink,
    view_only: bool,
    clipboard_enabled: bool,
) -> InputController<RecordingSink> {
    let mut controller = InputController::new(sink, view_only, clipboard_enabled);
    controller.mark_ready();
    controller
}

#[test]
fn ctrl_alt_delete_emits_exact_semantic_rfb_order() {
    let sink = RecordingSink::default();
    let mut controller = ready_controller(sink.clone(), false, false);

    controller.ctrl_alt_delete().unwrap();

    assert_eq!(
        sink.attempts(),
        [
            Attempt::Key {
                down: true,
                keysym: CONTROL_L,
            },
            Attempt::Key {
                down: true,
                keysym: ALT_L,
            },
            Attempt::Key {
                down: true,
                keysym: DELETE,
            },
            Attempt::Key {
                down: false,
                keysym: DELETE,
            },
            Attempt::Key {
                down: false,
                keysym: ALT_L,
            },
            Attempt::Key {
                down: false,
                keysym: CONTROL_L,
            },
        ]
    );
}

#[test]
fn ctrl_alt_delete_returns_first_failure_after_cleanup_attempts_and_clears_tracking() {
    let sink = RecordingSink::default().fail_with([
        (3, InputError::ClipboardDisabled),
        (4, InputError::QueueUnavailable),
    ]);
    let mut controller = ready_controller(sink.clone(), false, false);

    assert_eq!(
        controller.ctrl_alt_delete().unwrap_err(),
        InputError::ClipboardDisabled
    );
    assert_eq!(
        sink.attempts(),
        [
            Attempt::Key {
                down: true,
                keysym: CONTROL_L,
            },
            Attempt::Key {
                down: true,
                keysym: ALT_L,
            },
            Attempt::Key {
                down: true,
                keysym: DELETE,
            },
            Attempt::Key {
                down: false,
                keysym: DELETE,
            },
            Attempt::Key {
                down: false,
                keysym: ALT_L,
            },
            Attempt::Key {
                down: false,
                keysym: CONTROL_L,
            },
        ]
    );

    controller.release_all_keys().unwrap();
    assert_eq!(sink.attempts().len(), 6, "tracked set must be empty");
}

#[test]
fn key_up_updates_after_attempt_and_release_all_is_reverse_deterministic() {
    let sink = RecordingSink::default();
    let mut controller = ready_controller(sink.clone(), false, false);

    controller.key(true, 0x20).unwrap();
    controller.key(false, 0x20).unwrap();
    controller.key(true, 1).unwrap();
    controller.key(true, 3).unwrap();
    controller.key(true, 2).unwrap();
    controller.release_all_keys().unwrap();

    assert_eq!(
        sink.attempts(),
        [
            Attempt::Key {
                down: true,
                keysym: 0x20,
            },
            Attempt::Key {
                down: false,
                keysym: 0x20,
            },
            Attempt::Key {
                down: true,
                keysym: 1,
            },
            Attempt::Key {
                down: true,
                keysym: 3,
            },
            Attempt::Key {
                down: true,
                keysym: 2,
            },
            Attempt::Key {
                down: false,
                keysym: 3,
            },
            Attempt::Key {
                down: false,
                keysym: 2,
            },
            Attempt::Key {
                down: false,
                keysym: 1,
            },
        ]
    );
}

#[test]
fn tracked_keys_are_capped_at_the_exact_queue_capacity_and_release_stays_bounded() {
    let sink = RecordingSink::default();
    let mut controller = ready_controller(sink.clone(), false, false);

    for keysym in 0..VNC_QUEUE_CAPACITY as u32 {
        controller.key(true, keysym).unwrap();
    }
    assert_eq!(sink.attempts().len(), VNC_QUEUE_CAPACITY);

    controller.key(true, 0).unwrap();
    assert_eq!(sink.attempts().len(), VNC_QUEUE_CAPACITY + 1);
    assert_eq!(
        controller.key(true, VNC_QUEUE_CAPACITY as u32).unwrap_err(),
        InputError::PressedKeyLimit
    );
    assert_eq!(
        sink.attempts().len(),
        VNC_QUEUE_CAPACITY + 1,
        "the 257th distinct key must not reach the sink"
    );

    controller.key(false, 0).unwrap();
    controller.key(true, VNC_QUEUE_CAPACITY as u32).unwrap();
    let release_start = sink.attempts().len();
    controller.release_all_keys().unwrap();
    let attempts = sink.attempts();
    let releases = &attempts[release_start..];
    assert_eq!(releases.len(), VNC_QUEUE_CAPACITY);
    for (offset, attempt) in releases.iter().enumerate() {
        assert_eq!(
            attempt,
            &Attempt::Key {
                down: false,
                keysym: (VNC_QUEUE_CAPACITY - offset) as u32,
            }
        );
    }

    controller.release_all_keys().unwrap();
    assert_eq!(sink.attempts().len(), release_start + VNC_QUEUE_CAPACITY);
}

#[test]
fn production_sink_distinguishes_full_queue_from_disconnected_transport() {
    let (mut full, full_channels) = bounded_vnc_channels();
    for keysym in 0..VNC_QUEUE_CAPACITY as u32 {
        InputSink::key(&mut full, true, keysym).unwrap();
    }
    assert_eq!(
        InputSink::pointer(&mut full, 0, 0, 0).unwrap_err(),
        InputError::QueueUnavailable
    );
    drop(full_channels);

    let (mut disconnected_key, key_channels) = bounded_vnc_channels();
    drop(key_channels.command_rx);
    assert_eq!(
        InputSink::key(&mut disconnected_key, true, 1).unwrap_err(),
        InputError::TransportDisconnected
    );

    let (mut disconnected_pointer, pointer_channels) = bounded_vnc_channels();
    drop(pointer_channels.command_rx);
    assert_eq!(
        InputSink::pointer(&mut disconnected_pointer, 1, 2, 3).unwrap_err(),
        InputError::TransportDisconnected
    );

    let (mut disconnected_clipboard, clipboard_channels) = bounded_vnc_channels();
    drop(clipboard_channels.command_rx);
    assert_eq!(
        InputSink::send_clipboard(&mut disconnected_clipboard, "text".to_owned()).unwrap_err(),
        InputError::TransportDisconnected
    );
}

#[test]
fn release_all_attempts_every_key_and_clears_tracking_after_failures() {
    let sink = RecordingSink::default().fail_with([
        (5, InputError::QueueUnavailable),
        (6, InputError::ClipboardDisabled),
    ]);
    let mut controller = ready_controller(sink.clone(), false, false);
    controller.key(true, 1).unwrap();
    controller.key(true, 2).unwrap();
    controller.key(true, 3).unwrap();

    assert_eq!(
        controller.release_all_keys().unwrap_err(),
        InputError::QueueUnavailable
    );
    assert_eq!(
        &sink.attempts()[3..],
        [
            Attempt::Key {
                down: false,
                keysym: 3,
            },
            Attempt::Key {
                down: false,
                keysym: 2,
            },
            Attempt::Key {
                down: false,
                keysym: 1,
            },
        ]
    );

    controller.release_all_keys().unwrap();
    assert_eq!(sink.attempts().len(), 6, "failed releases must still clear");
}

#[test]
fn release_owned_input_attempts_pointer_and_all_keys_returns_first_error_and_clears_tracking() {
    let sink = RecordingSink::default().fail_with([
        (3, InputError::QueueUnavailable),
        (4, InputError::ClipboardDisabled),
    ]);
    let mut controller = ready_controller(sink.clone(), false, false);
    controller.key(true, 1).unwrap();
    controller.key(true, 2).unwrap();

    assert_eq!(
        controller
            .release_owned_input(Some((123, 234)))
            .unwrap_err(),
        InputError::QueueUnavailable,
        "the pointer failure is returned after key cleanup is still attempted"
    );
    assert_eq!(
        sink.attempts(),
        [
            Attempt::Key {
                down: true,
                keysym: 1,
            },
            Attempt::Key {
                down: true,
                keysym: 2,
            },
            Attempt::Pointer {
                buttons: 0,
                x: 123,
                y: 234,
            },
            Attempt::Key {
                down: false,
                keysym: 2,
            },
            Attempt::Key {
                down: false,
                keysym: 1,
            },
        ]
    );

    controller.release_owned_input(None).unwrap();
    assert_eq!(
        sink.attempts().len(),
        5,
        "failed key releases must still clear bounded tracking"
    );
}

#[test]
fn focus_loss_and_view_only_activation_release_keys_and_view_only_sticks_on_error() {
    let sink = RecordingSink::default().fail_with([(4, InputError::QueueUnavailable)]);
    let mut controller = ready_controller(sink.clone(), false, false);

    controller.key(true, 1).unwrap();
    controller.release_owned_input(None).unwrap();
    controller.key(true, 2).unwrap();
    assert_eq!(
        controller.set_view_only(true).unwrap_err(),
        InputError::QueueUnavailable
    );
    assert_eq!(controller.key(true, 3).unwrap_err(), InputError::ViewOnly);
    assert_eq!(
        sink.attempts(),
        [
            Attempt::Key {
                down: true,
                keysym: 1,
            },
            Attempt::Key {
                down: false,
                keysym: 1,
            },
            Attempt::Key {
                down: true,
                keysym: 2,
            },
            Attempt::Key {
                down: false,
                keysym: 2,
            },
        ]
    );
}

#[test]
fn interactive_actions_are_gated_but_release_all_is_always_available() {
    let sink = RecordingSink::default();
    let mut controller = InputController::new(sink.clone(), false, true);

    assert_eq!(controller.key(true, 1).unwrap_err(), InputError::NotReady);
    assert_eq!(
        controller.pointer(1, 10, 20).unwrap_err(),
        InputError::NotReady
    );
    assert_eq!(
        controller.ctrl_alt_delete().unwrap_err(),
        InputError::NotReady
    );
    assert_eq!(
        controller.send_clipboard("text".to_owned()).unwrap_err(),
        InputError::NotReady
    );
    assert_eq!(
        controller
            .receive_clipboard()
            .err()
            .expect("receive before Ready must fail"),
        InputError::NotReady
    );
    controller.release_all_keys().unwrap();
    controller.release_owned_input(Some((10, 20))).unwrap();
    assert_eq!(
        sink.attempts(),
        [Attempt::Pointer {
            buttons: 0,
            x: 10,
            y: 20,
        }],
        "targeted pointer cleanup is a recovery action before Ready"
    );

    controller.mark_ready();
    controller.set_view_only(true).unwrap();
    assert_eq!(controller.key(true, 1).unwrap_err(), InputError::ViewOnly);
    assert_eq!(
        controller.pointer(1, 10, 20).unwrap_err(),
        InputError::ViewOnly
    );
    assert_eq!(
        controller.ctrl_alt_delete().unwrap_err(),
        InputError::ViewOnly
    );
    assert_eq!(
        controller.send_clipboard("text".to_owned()).unwrap_err(),
        InputError::ViewOnly
    );
    assert_eq!(
        controller
            .receive_clipboard()
            .err()
            .expect("receive in view-only must fail"),
        InputError::ViewOnly
    );
    controller.release_all_keys().unwrap();
    controller.release_owned_input(Some((30, 40))).unwrap();
    assert_eq!(
        sink.attempts(),
        [
            Attempt::Pointer {
                buttons: 0,
                x: 10,
                y: 20,
            },
            Attempt::Pointer {
                buttons: 0,
                x: 30,
                y: 40,
            },
        ],
        "targeted pointer cleanup remains available in view-only mode"
    );
}

#[test]
fn clipboard_is_default_off_and_exactly_one_mib_is_the_hard_ceiling() {
    let disabled_sink = RecordingSink::default();
    let mut disabled = ready_controller(disabled_sink.clone(), false, false);
    assert_eq!(
        disabled.send_clipboard("text".to_owned()).unwrap_err(),
        InputError::ClipboardDisabled
    );
    assert!(disabled_sink.attempts().is_empty());

    let sink = RecordingSink::default();
    let mut enabled = ready_controller(sink.clone(), false, true);
    enabled.send_clipboard("a".repeat(CLIPBOARD_LIMIT)).unwrap();
    assert_eq!(sink.attempts(), [Attempt::Clipboard(CLIPBOARD_LIMIT)]);

    assert_eq!(
        enabled
            .send_clipboard("a".repeat(CLIPBOARD_LIMIT + 1))
            .unwrap_err(),
        InputError::ClipboardTooLarge
    );
    assert_eq!(
        sink.attempts(),
        [Attempt::Clipboard(CLIPBOARD_LIMIT)],
        "oversized text must be rejected before the VNC queue"
    );
}

#[test]
fn tightened_clipboard_limit_rejects_before_queue_and_keeps_controller_usable() {
    let sink = RecordingSink::default();
    let mut controller =
        InputController::with_clipboard_limit(sink.clone(), false, true, 4).unwrap();
    controller.mark_ready();

    controller.send_clipboard("1234".to_owned()).unwrap();
    assert_eq!(sink.attempts(), [Attempt::Clipboard(4)]);

    assert_eq!(
        controller.send_clipboard("12345".to_owned()).unwrap_err(),
        InputError::ClipboardTooLarge
    );
    assert_eq!(sink.attempts(), [Attempt::Clipboard(4)]);

    controller.key(true, 0x41).unwrap();
    assert_eq!(
        sink.attempts(),
        [
            Attempt::Clipboard(4),
            Attempt::Key {
                down: true,
                keysym: 0x41,
            },
        ]
    );
}

#[test]
fn controller_never_accepts_a_relaxed_clipboard_limit() {
    let result = InputController::with_clipboard_limit(
        RecordingSink::default(),
        false,
        true,
        CLIPBOARD_LIMIT + 1,
    );
    assert!(matches!(result, Err(InputError::InvalidClipboardLimit)));
}

#[test]
fn remote_clipboard_is_strict_bounded_replacing_one_shot_ephemeral_text() {
    assert_eq!(
        ClipboardText::try_from(vec![0x66, 0x80])
            .err()
            .expect("malformed UTF-8 must fail"),
        InputError::InvalidClipboardText
    );
    assert_eq!(
        ClipboardText::try_from(vec![b'a'; CLIPBOARD_LIMIT + 1])
            .err()
            .expect("oversized remote clipboard must fail"),
        InputError::ClipboardTooLarge
    );

    let sink = RecordingSink::default();
    let mut controller = ready_controller(sink, false, true);
    controller.buffer_remote_clipboard(ClipboardText::try_from(b"first".to_vec()).unwrap());
    controller.buffer_remote_clipboard(ClipboardText::try_from(b"second".to_vec()).unwrap());

    let received = controller.receive_clipboard().unwrap().unwrap();
    assert_eq!(received.as_str(), "second");
    assert!(controller.receive_clipboard().unwrap().is_none());

    controller.buffer_remote_clipboard(ClipboardText::try_from(b"close me".to_vec()).unwrap());
    controller.clear_session().unwrap();
    controller.mark_ready();
    assert!(controller.receive_clipboard().unwrap().is_none());
}

#[test]
fn public_input_actions_are_semantic_and_clipboard_payloads_are_not_debuggable() {
    let cases = trybuild::TestCases::new();
    cases.compile_fail("tests/ui/raw_input_forward.rs");
    cases.compile_fail("tests/ui/clipboard_debug.rs");
    cases.compile_fail("tests/ui/legacy_split_cleanup.rs");

    fn assert_reviewed_semantic_action(action: InputAction) {
        match action {
            InputAction::Key { .. }
            | InputAction::Pointer { .. }
            | InputAction::ReleaseOwnedInput { .. }
            | InputAction::CtrlAltDelete
            | InputAction::ReleaseAllKeys
            | InputAction::SetViewOnly(_)
            | InputAction::SendClipboard(_)
            | InputAction::ReceiveClipboard => {}
        }
    }

    for action in [
        InputAction::Key {
            down: true,
            keysym: 1,
        },
        InputAction::Pointer {
            buttons: 0,
            x: 0,
            y: 0,
        },
        InputAction::ReleaseOwnedInput {
            pointer_position: Some((0, 0)),
        },
        InputAction::CtrlAltDelete,
        InputAction::ReleaseAllKeys,
        InputAction::SetViewOnly(true),
        InputAction::SendClipboard(String::new()),
        InputAction::ReceiveClipboard,
    ] {
        assert_reviewed_semantic_action(action);
    }
}
