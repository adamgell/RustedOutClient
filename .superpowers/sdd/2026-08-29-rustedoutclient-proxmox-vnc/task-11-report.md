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
