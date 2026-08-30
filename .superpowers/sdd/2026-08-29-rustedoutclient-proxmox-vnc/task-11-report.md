# Task 11 implementation report

Date: 2026-08-30
Worktree: `/Users/Adam.Gell/repo/worktrees/RustedOutClient-feature-proxmox-console`
Branch: `feature/proxmox-console-foundation`
Required base: `1545bb7400bb5d3be03665ac4bfccd22c7874f18`
Commit message: `feat: add RustedOutClient session workspace`

## Starting evidence

- `git rev-parse HEAD` returned exactly `1545bb7400bb5d3be03665ac4bfccd22c7874f18` before editing.
- The linked worktree was clean before Task 11 changes.
- The baseline `cargo test --all-targets` passed before implementation: 96 library tests and all pre-Task-11 integration targets were green.
- The Task 11 brief, controller context, global constraints, and the complete named design sections were read before editing.
- No stash operation was run. Task 8 stash `0d95b4f403abb215f8d6d9f8e21d64201d60e76a` was not applied, edited, dropped, recreated, or included.

## Test-first RED evidence

The two required contract targets were created before their production interfaces.

1. `cargo test --test app_state_contract`
   - Result: exit 101 (RED).
   - Actual failure class: compile-time unresolved production imports/types from `rustedoutclient::app`, including the pure state, action availability, semantic command sink, clipboard adapter/effect, dispatch outcome, and UI action interfaces.
   - No test reached GREEN through a placeholder implementation.

2. `cargo test --test display_resize_contract`
   - Result: exit 101 (RED).
   - Actual failure class: compile-time unresolved production resize interfaces, including `DesktopSize`, resize protocol outcomes/status, normalization, SetEncodings/SetDesktopSize encoding, ExtendedDesktopSize parsing, and session commands/events.

The RED tests covered action gating, inventory/favorites/tabs, deterministic state, checked framebuffer application, clipboard non-retention, nonblocking queue pressure, exact wire bytes, malformed parser inputs, debounce, one-in-flight/one-pending policy, Pending-versus-Applied, terminal capability outcomes, timeout, retry, toggle, and reconnect safety.

### Controller SetDesktopSize correction

The original two expected vectors in the RED fixture contained 26 bytes. After the controller correction, the extra zero `u16` before screen width was removed from both vectors. The test now explicitly asserts `expected.len() == 24`.

Production was implemented to the corrected protocol layout and never made to match the incorrect 26-byte fixture:

```text
type | pad | width | height | count | pad | id | x | y | width | height | flags
 1      1      2       2        1      1     4    2   2      2        2       4
```

Total: exactly 24 bytes.

## Implementation

### Native session workspace

- Replaced the placeholder monolithic `src/app.rs` with `src/app/mod.rs`, `state.rs`, `actions.rs`, and `view.rs`.
- Starts the existing production session manager immediately after a valid private configuration loads, allowing stale cached inventory to render while the verified SSH master and live inventory warm in the worker.
- Missing and invalid configuration states are directional and do not add endpoint, username, password, or arbitrary connection fields.
- Uses a 1280x800 default native window with a 960x640 minimum.
- Implements searchable alias/name/VMID inventory, favorites-first ordering, running/stopped and live/stale text, two-session-bounded native tabs, focus-existing behavior, reconnect tab pruning, local Fit/1:1/fullscreen controls, and redacted diagnostics.
- Background lifecycle events do not steal the selected tab.

### Intentional graphite instrument-bay UI

The UI uses the controller-approved tokens exactly:

- Canvas black `#0D1117`
- Chassis graphite `#171C23`
- Panel steel `#242B35`
- Primary text `#E6EDF3`
- Proxmox orange `#E57000`
- Ready/resize cyan `#4FB7C5`

It renders the required menu bar, 248-280 px rack-index sidebar, restrained session bus, compact action strip, recessed console instrument bay with corner brackets and visible keyboard focus, and one-line horizontally scrollable telemetry rail. Status is textual as well as colored and includes profile, node, inventory age/source, VMID, VM name, phase, scale mode, guest dimensions, resize state, view-only state, clipboard state, and queue pressure.

All required Session menu labels are exact. Dynamic Resolution is exposed in View and the toolbar. `Open in TigerVNC`/`TigerVNC` is rendered disabled and typed unavailable; no Task 12 process, listener, password file, or alternate transport exists.

### Pure state and nonblocking actions

- `AppState::apply(AppEvent)` performs deterministic in-memory transitions and returns one-shot effects; it performs no I/O.
- Framebuffer geometry and byte counts are checked before mutation. A multi-rectangle update validates every rectangle before committing any pixels.
- Per-session images are bounded, the render-state cap is two sessions, and framebuffer Debug output contains dimensions/byte count/revision only, never pixels.
- Egui textures are reused while dimensions remain unchanged and replaced only on a size change.
- Fit scales locally; 1:1 maps framebuffer pixels to backing pixels. Pointer coordinates are converted through the displayed image rectangle and clamped to valid guest coordinates.
- Pointer release outside the image and focus loss emit semantic recovery actions; keyboard modifiers are tracked and released through the Task 10 semantic boundary.
- UI-to-worker commands use only nonblocking `try_send`. Full and disconnected app queues produce typed Busy/Disconnected outcomes. The UI drains at most the existing 256-event capacity per frame and never awaits worker capacity.
- A delayed/unconsumed bounded fake proves immediate Full behavior. One thousand viewport dispatches complete synchronously without awaiting a worker.
- The app receives and emits semantic `AppCommand`/`InputAction` values only. No raw `VncCommand` is exposed in the app modules.

### Explicit clipboard adapter boundary

- Task 11 guest-console and diagnostics clipboard operations exist only behind `ClipboardAdapter`.
- `SystemClipboard` opens the host clipboard only for an explicit Send Clipboard, Receive Clipboard result, or Copy Diagnostics action.
- Send reads host text once and immediately moves it into the semantic input action.
- Remote clipboard text bypasses `AppState` in a non-Debug one-shot `UiEffect`, is written once through the adapter, and is dropped immediately after the attempt.
- There is no polling, automatic synchronization, persistence, serialization, logging, diagnostics inclusion, or state retention of clipboard text.
- Automated tests use only a fake clipboard and synthetic strings; they never touch the real host clipboard.

### Dynamic guest resolution

- SetEncodings advertises `[16, 5, 1, 0, -223, -308, 7]` through the existing sole RFB writer.
- SetDesktopSize message 251 is exactly 24 bytes with one id-0, origin-zero, flags-zero screen.
- Backing-pixel viewport requests reject raw sizes below 640x480, dimensions above 8192, and raw pixel counts above 33,554,432 before queue entry, then round each accepted dimension down to a multiple of eight.
- The production framebuffer parser reads ExtendedDesktopSize reason/result and the bounded screen payload. The wire count is one byte and at most 255 screens; payload length is checked before a bounded allocation.
- The supported subset is exactly one screen, id 0, origin `(0,0)`, zero flags, nonzero bounded dimensions matching the rectangle. Structurally valid other layouts become nonterminal Unsupported outcomes; truncation, contradiction, overflow, duplicate IDs, bad padding, bad reason/result, and invalid bounds are terminal protocol errors.
- Client reason/result success emits Forwarded/Pending only and does not resize the framebuffer. A production-parser unit test sends a forwarded response followed by a server-driven response and proves that only the latter resizes and emits the actual DesktopSize.
- The session worker owns a 250 ms debounce, one in-flight request, one newest pending replacement, and a two-second actual-size deadline.
- A matching later DesktopSize/ExtendedDesktopSize is Applied and may release the already-debounced newest replacement. Forwarded remains Pending. Rejected, Unsupported, and TimedOut are nonterminal, stop automatic follow-ups, preserve the healthy Ready session and local Fit, and expose Retry.
- Turning Dynamic Resolution off clears automatic debounce/pending bookkeeping without disconnecting. Turning it back on or Retry re-arms the current stable viewport.
- Dynamic Resolution defaults on for each new native session. Reconnect carries the current sticky view-only and dynamic-resolution state; a different new native session defaults dynamic resolution on.
- No timer thread, parser, queue, transport, direct TCP path, VM hardware mutation, `qm set`, or guest-driver action was added.

### Task 1-10 boundary preservation

- Existing strict OpenSSH/known-host, fixed-command, trusted-proxy, VNC-auth, protocol-limit, event-pressure, key-release, clipboard-slot, close-deadline, owned-process cleanup, private config/cache, and content-free public-error paths remain in place.
- The only test-fixture adjustment outside Task 11 contracts changes two synthetic RFB handshake consumes from 58 to 62 bytes because SetEncodings grew by exactly four bytes when `-308` was added. This keeps existing real synthetic key-release and malformed-first-frame tests aligned with the production handshake.
- No dependency or lockfile change was required.
- No legacy rollback artifact was modified.

## GREEN and gate evidence

Required final sequence, run on the final uncommitted Task 11 tree:

| Command | Result |
|---|---|
| `cargo fmt --all` | exit 0 |
| `cargo test --test app_state_contract` | exit 0; 7 passed |
| `cargo test --test display_resize_contract` | exit 0; 8 passed |
| `cargo test --test input_contract` | exit 0; 13 passed, including compile-fail surface tests |
| `cargo test --test session_manager_contract` | exit 0; 23 passed |
| `cargo test --all-targets` | exit 0; 97 library + 149 integration = 246 passed, 0 failed |
| `cargo clippy --all-targets -- -D warnings` | exit 0 |
| `cargo build --release` | exit 0 |
| `git diff --check` | exit 0 |

Additional focused evidence:

- `cargo test --lib vnc::client::tests::production_parser_keeps_forwarded_resize_pending_until_later_server_size -- --exact`: 1 passed.
- The existing production-path key-release fixture initially exposed its old 58-byte handshake assumption after SetEncodings grew by four bytes. After correcting the synthetic consume to 62 bytes, the exact failed test passed and the final all-target run was fully green.

## Privacy and security review

- No live Proxmox endpoint was accessed and no VM was queried or changed.
- No real endpoint, credential, ticket, clipboard value, private key, environment dump, fingerprint, raw stderr, or guest pixel content was added to code, tests, diagnostics, logs, or this report.
- Synthetic endpoint strings use `.invalid` names.
- App diagnostics include only approved product/platform/profile/node/VM/state categories. Framebuffer pixels and clipboard values are structurally absent.
- Source scans confirm the app layer contains no `VncCommand`, `TcpListener`, `qm set`, or viewer-process implementation.
- No direct VNC host/port/password path, listener, timer thread, alternate parser, or second command/event queue was introduced.
- No long-running GUI process was started.

## Self-review and concerns

- Reviewed the complete diff for protocol field offsets, checked arithmetic, bounded allocation, action gating, state determinism, focus behavior, reconnect state, clipboard lifetime, texture reuse, queue pressure, and Task 12 exclusion.
- Corrected the controller-identified SetDesktopSize fixtures to 24 bytes and confirmed production positions independently rather than conforming production to the former 26-byte vectors.
- Corrected the existing synthetic RFB fixture's handshake length after the all-target gate exposed the SetEncodings growth.
- Added a production parser integration unit test rather than relying only on a standalone payload helper test.
- Final source formatting, warning denial, release compilation, full tests, and diff whitespace checks are clean.

Remaining acceptance boundary: native visual inspection, real Windows pointer/keyboard/CAD behavior, live inventory timing, two live sessions, guest resize support/timeout behavior, repeated live cleanup, and explicit real clipboard transfer are controller-run Task 11/native acceptance. They were intentionally not attempted because this task forbids live Proxmox/VM/real clipboard access and long-running GUI processes. Task 12 TigerVNC fallback remains intentionally unavailable.

# Task 11 fix round 1 report

Date: 2026-08-30
Fix brief: `task-11-fix-round-1.md`
Required fix base: `e1b4a37f0540c1945b715ca22394083166276e92`
Implementation commit message: `fix: harden native session workspace`
Final implementation commit SHA: `dd4f69b0d0b02a95409021ad21a9d4042cb2b510`

This section supersedes the original Task 11 report where fix-round behavior or test counts differ. The original implementation evidence above remains the record for the initial Task 11 commit.

## Fix-round starting evidence

- Before editing, `git rev-parse HEAD` returned exactly `e1b4a37f0540c1945b715ca22394083166276e92`.
- The linked worktree was clean and remained on `feature/proxmox-console-foundation`.
- The complete global constraints, Task 11 brief, Task 11 controller context, Task 11 fix-round-1 brief, and linked design sections were read before editing.
- No subagent or delegated task was created.
- No live Proxmox endpoint, VM, guest driver, host clipboard, or long-running GUI process was accessed.
- No dependency was added or changed.
- No stash command was run. Parked Task 8 stash 0 stayed exactly `0d95b4f403abb215f8d6d9f8e21d64201d60e76a` with subject `task8-parser-qa-pending-program-access`.

## Fix-round RED evidence

All regression tests were added before their corresponding production changes. The observed failures were:

1. Native close coordinator
   - Command: `cargo test --lib close_coordinator_tests`
   - Result: exit 101.
   - Actual failure: unresolved imports `CloseCoordinator` and `NativeCloseAction` in `src/app/mod.rs`.

2. Resize request lifetime across off/on, Retry, and timeout
   - Command: `cargo test --test display_resize_contract`
   - Result: exit 101; 10 passed and 2 failed.
   - `off_on_and_retry_do_not_replace_an_unanswered_transmitted_resize` observed two wire requests instead of one: the old `1600x896` request and an incorrectly concurrent `1920x1080` request.
   - `unanswered_timeout_keeps_old_outcome_correlated_and_releases_only_newest_retry` likewise observed an incorrectly concurrent `1992x1080` request after the unanswered old request timed out.
   - Follow-up exact test `matching_actual_geometry_applies_but_keeps_unanswered_protocol_correlation` failed because the expected Applied snapshot was never emitted when actual geometry arrived before the protocol outcome.

3. Modifier synchronization, per-session ownership, and visible hit testing
   - Command: `cargo test --lib app::view::input_tests`
   - Result: exit 101 with 10 compile errors.
   - Actual failures included missing pure keyboard/pointer collectors, missing `InputOwnership`, missing explicit `session_id` fields, and missing targeted `ReleasePointer`.
   - Later self-review test `toolbar_and_menu_actions_release_console_ownership_before_the_action` failed because CAD/Fit/Dynamic Resolution/Diagnostics did not all enter the shared cleanup-first path.

4. Viewport acknowledgement under app-command pressure
   - Command: exact `viewport_acknowledgement_waits_for_queue_acceptance_and_retries_only_the_newest_value` test.
   - Result: exit 101; 0 passed and 1 failed.
   - Actual assertion: local state contained `Some(BackingViewport { width: 2599, height: 1899 })` after every manager send returned Full; expected acknowledged viewport was `None`.

5. Server-driven unsupported ExtendedDesktopSize geometry
   - Command: exact `extended_desktop_size_distinguishes_pending_actual_rejected_and_unsupported` test.
   - Result: exit 101 with four compile errors.
   - Actual failures: missing `ExtendedDesktopSize::ServerUnsupported` and `ResizeProtocolOutcome::ServerUnsupported` variants.

6. Partial framebuffer upload planning
   - Command: exact `framebuffer_upload_plans_are_bounded_coalesced_and_acknowledged_transactionally` test.
   - Result: exit 101 with 13 compile errors.
   - Actual failures: missing non-Debug upload kind/plan and missing plan/acknowledgement state methods.

No production code was changed to make the corrected SetDesktopSize fixture 26 bytes. The exact Task 11 encoding remains 24 bytes in the required order: `type,pad,w,h,count,pad,id,x,y,w,h,flags`.

## Fix-round implementation

### Important 1: coordinated native close

- Added a small nonblocking close coordinator with explicit NotRequested, Pending, Enqueued, and SenderDisconnected shutdown states.
- Every close request while cleanup is incomplete emits `ViewportCommand::CancelClose`.
- Full retains one pending logical shutdown intent and retries on later frames; a successful enqueue is never duplicated.
- A disconnected command sender does not release the manager. The manager is retained until the event receiver reports Disconnected, proving the worker has completed and the runtime can be released.
- Only after event-channel completion and manager release does the coordinator emit one final `ViewportCommand::Close`.
- The eframe callback contains no await, sleep, helper thread, or unowned cleanup task.

### Important 2: correlated resize lifecycle

- Split transmitted resize lifecycle into AwaitingOutcome versus Forwarded protocol state, with independent actual-geometry observation and timeout state.
- Dynamic Resolution off clears only unsent debounce/replacement/retry intent. It never erases a transmitted request.
- Dynamic Resolution on and Retry re-arm exactly one current desired target. One thousand viewport changes still collapse to the newest normalized target.
- An unanswered timed-out request remains correlated and blocks another wire request until its late protocol outcome is consumed.
- A matching actual DesktopSize can truthfully become Applied before the protocol outcome, but the unresolved protocol lifecycle remains retained and blocks replacement release.
- Forwarded without actual geometry remains Pending. The two-second no-change deadline remains distinct from protocol correlation.
- A late old-size update cannot apply a different newest transmitted target. Repeated same-target Retry applies only after actual matching geometry exists.
- State remains bounded to one transmitted lifecycle and one newest replacement; no sequence history, queue, parser, transport, or timer thread was added.

### Important 3: modifier-only synchronization

- Preserved event-local modifier synchronization before each ordinary key.
- Added one final aggregate synchronization from the frame's current egui modifiers while the console owns keyboard focus.
- Shift, Control, Alt, and Command press/release transitions are emitted once; unchanged frames are deduplicated.
- Focus loss still clears tracked modifiers through targeted semantic `InputAction::FocusLost`, which retains Task 10 key-release policy.

### Important 4: per-session input ownership

- Replaced global pointer/modifier state with one bounded `InputOwnership` record carrying the owning `SessionId`, keyboard focus, modifier bits, held pointer buttons, and last guest coordinates.
- Key, pointer, zero-button pointer release, and FocusLost UI actions now carry an explicit target session all the way to `AppCommand::SendInput`.
- Added semantic `InputAction::ReleasePointer { x, y }`; it can emit only a zero-button recovery event and remains available before Ready and in view-only mode. Fresh key/pointer input remains Ready+writable gated.
- Tab switch, console-widget loss, app/window loss, owner-tab removal, close/reconnect/view-only controls, and every other toolbar/menu action release the outgoing owner before the new action.
- All five egui pointer buttons are covered. Release outside the visible image is sent to the session where the press began. Incoming sessions receive no synthetic cleanup.

### Important 5: viewport backpressure

- `BackingViewport` now means successfully acknowledged by the manager queue.
- The app sends `AppCommand::ViewportChanged` first and updates local acknowledgement only on `DispatchOutcome::Sent`.
- Full leaves acknowledgement unchanged, reports Busy immediately, and lets rendering retry its current latest measurement on a later frame.
- A deterministic 1,000-change Full-pressure test proves old values are discarded and only `2599x1899` is eventually accepted once, returning queue state to Ready.

### Important 6: valid unsupported server geometry

- Added a typed distinction between client-request Unsupported and server-driven `ServerUnsupported` geometry.
- Structurally valid bounded reason-0/reason-2 multi-screen or flagged layouts retain the aggregate rectangle-header dimensions.
- Production flushes prior dirty state, resizes the framebuffer transactionally, emits actual DesktopSize, emits nonterminal ServerUnsupported capability state, and requests a nonincremental full update at the new dimensions.
- Production tests cover multi-screen grow, flagged reason-2 shrink, a following raw rectangle in newly added space, exact full/incremental request dimensions, and no client resize in flight.
- Client reason rejection/unsupported remains non-mutating. Existing malformed, zero-screen, contradictory, duplicate-ID, bounds, count, and limit failures remain typed terminal errors.

### Important 7: partial unchanged-size uploads

- Each framebuffer image now retains one bounded dirty rectangle. Creation and dimension changes mark the full image dirty; valid updates union into one region.
- Upload plans stage only exact dirty rows. The plan owns ephemeral pixels but implements neither Debug nor Display.
- Unchanged texture dimensions use egui 0.31 `TextureHandle::set_partial`; new/resized textures may use one full upload.
- Dirty metadata clears only after submission acknowledgement. Revision matching prevents an old acknowledgement from clearing a later update.
- A maximum `8192x4096` framebuffer with a 1x1 update produces a 1x1 four-byte partial plan, not a full-size staging allocation.
- Disjoint coalescing, row stride, later-update retention, resize-full upload, and two-session independence are covered.

### Important 8: visible 1:1 input surface

- Pointer initiation uses `image_rect ∩ instrument_inner_rect ∩ ui.clip_rect()`.
- Guest coordinate conversion still uses the original full image rectangle, preserving correct mapping for cropped 1:1 content.
- Hidden presses and movement are rejected. A press begun in the visible surface still emits targeted zero-button cleanup when released outside.

## Focused GREEN evidence

Post-format focused checks on the final implementation:

| Command/focus | Result |
|---|---|
| `cargo test --lib app::close_coordinator_tests` | 5 passed |
| `cargo test --lib app::view::input_tests` | 7 passed |
| `cargo test --lib server_unsupported` | 2 passed |
| Exact viewport Full/retry pressure test | 1 passed |
| Exact maximum-geometry four-byte upload test | 1 passed |
| Exact actual-before-protocol resize correlation test | 1 passed |
| Exact input recovery-gating test | 1 passed |

## Required final gate sequence

This table supersedes the original Task 11 gate table above. It was run after the final production and test change:

| Command | Result |
|---|---|
| `cargo fmt --all` | exit 0 |
| `cargo test --test app_state_contract` | exit 0; 12 passed |
| `cargo test --test display_resize_contract` | exit 0; 14 passed |
| `cargo test --test input_contract` | exit 0; 13 passed, including two nested compile-fail fixtures |
| `cargo test --test session_manager_contract` | exit 0; 23 passed |
| `cargo test --all-targets` | exit 0; 112 library + 160 integration = 272 passed, 0 failed; four nested trybuild fixtures also passed |
| `cargo clippy --all-targets -- -D warnings` | exit 0; no warnings |
| `cargo build --release` | exit 0 |
| `git diff --check` | exit 0 |

## Fix-round changed files

- `src/app/actions.rs` — explicit input targets, pointer-recovery semantic dispatch, post-send viewport acknowledgement.
- `src/app/mod.rs` — native close coordinator and manager-completion integration.
- `src/app/state.rs` — bounded dirty union, partial/full upload plans, transactional upload acknowledgement.
- `src/app/view.rs` — per-session ownership, aggregate modifiers, cleanup-first UI actions, visible hit intersection, partial texture submission, focused tests.
- `src/connection.rs` — typed server-driven unsupported resize outcome.
- `src/session/events.rs` — semantic zero-button pointer recovery action.
- `src/session/manager.rs` — correlated transmitted resize lifecycle and unconditional server capability outcome handling.
- `src/vnc/client.rs` — reason-aware ExtendedDesktopSize parsing, real unsupported geometry application, production tests.
- `src/vnc/input.rs` — zero-button pointer recovery method without fresh-input permission.
- `tests/app_state_contract.rs` — viewport pressure, targeted cleanup, dirty/upload state tests.
- `tests/display_resize_contract.rs` — paused-time resize correlation and server-unsupported tests.
- `tests/input_contract.rs` — recovery pointer gating and semantic action coverage.
- `tests/session_manager_contract.rs` — semantic pointer-recovery fake route.
- `.superpowers/sdd/2026-08-29-rustedoutclient-proxmox-vnc/task-11-report.md` — this complete fix-round evidence.

Cargo manifests and lockfiles are unchanged. No protected rollback artifact appears in the changed-file list.

## Fix-round privacy, security, and boundary review

- The app layer still has no raw `VncCommand`, direct TCP listener, arbitrary process launch, `qm set`, endpoint/password form, or Task 12 implementation.
- New UI actions remain semantic; the only pointer recovery addition can emit buttons=0.
- Existing queue capacities, two-native-session cap, framebuffer ceilings, parser allocation bounds, SSH transport, host verification, ticket lifetime, key release, cleanup deadlines, proxy/master ownership, clipboard one-shot behavior, and content-free public errors remain intact.
- Guest pixels remain excluded from Debug, diagnostics, persistence, serialization, hashing, and errors. Upload staging is ephemeral and non-Debug.
- Clipboard production code was not exercised; tests use the existing fake adapter and synthetic non-secret text only.
- No real credential, clipboard value, private URL, endpoint, ticket, key, host fingerprint, environment dump, raw stderr, or guest image was introduced.
- Task 12 remains a disabled typed-not-available action only.
- Task 8 stash 0 remains unchanged at `0d95b4f403abb215f8d6d9f8e21d64201d60e76a`.
- No live infrastructure action and no GUI smoke was run.

## Fix-round self-review against all eight findings

1. Close: CancelClose repeats while incomplete; Full retries one intent; manager/runtime survives until event disconnect; final Close occurs once after release.
2. Resize: on-wire state survives toggle/retry/timeout; actual and protocol outcomes are independent; one unresolved lifecycle plus one newest target is bounded; late old geometry cannot resolve a different current target.
3. Modifiers: pure modifier frames and ordinary-key ordering are exact and deduplicated.
4. Ownership: cleanup is explicit and outgoing-session targeted across tab, widget, app, menu/toolbar, close/reconnect, terminal/view-only, and outside-release transitions.
5. Viewport: Full cannot advance acknowledged state; the latest measurement retries nonblockingly and restores Ready after acceptance.
6. Geometry: valid unsupported server layouts advance aggregate dimensions and full-update bounds; client outcomes do not mutate; malformed inputs remain terminal.
7. Upload: unchanged-size dirties stage and submit only the coalesced region; acknowledgements are revision-safe; pixels remain private.
8. Hit testing: only the visible intersection initiates input while original image geometry maps cropped coordinates.

Remaining concern/acceptance boundary: visual polish, platform-native close behavior, live Windows guest input, real QEMU/guest ExtendedDesktopSize behavior, two concurrent live sessions, real host clipboard transfer, and repeated live cleanup remain controller-run native acceptance. They were intentionally not attempted under this fix brief's safety boundaries. No known automated-test or static-analysis concern remains.
