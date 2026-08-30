# Task 13 implementation report

## Result

Task 13 is complete from exact BASE
`1a1d8d1723cf0b1e2e79f801203498feb4905245`.

Implementation commit:
`5b299cee096ed43e8b1b4ee04ede079845078511`
(`test: prove native Proxmox console lifecycle`).

This report is the permitted evidence-only follow-up. No commit was amended or
pushed.

The implementation provides typed redacted diagnostics, monotonic session
phase telemetry, functional live-only `list`, native first-frame `probe`, one
typed GUI startup request for native or explicitly selected TigerVNC, and a
deterministic offline RFB/session lifecycle acceptance suite. Production SSH,
transport, protocol, capacity, input, resize, clipboard, fallback, and cleanup
boundaries from Tasks 1-12 remain unchanged.

## Scope and safety

- Worktree: `/Users/Adam.Gell/repo/worktrees/RustedOutClient-feature-proxmox-console`
- Starting HEAD: `1a1d8d1723cf0b1e2e79f801203498feb4905245`
- Implementation HEAD: `5b299cee096ed43e8b1b4ee04ede079845078511`
- No subagent or delegated worker was used.
- No live Proxmox endpoint or VM was contacted or changed.
- No real RustedOutClient/legacy configuration body, host clipboard, or
  TigerVNC viewer was read or invoked.
- No GUI acceptance process was started.
- No Task 8 stash/corpus was inspected, applied, dropped, or modified.
- No Task 14 work, fuzz target, production fixture route, endpoint override,
  backend selector, arbitrary SSH executable, or native listener was added.

## Tests-first RED evidence

### Baseline

Before Task 13 edits:

- `cargo build`: PASS.
- The first untouched `cargo test --all-targets` run had one transient existing
  readiness-timing failure in
  `ssh::stream::tests::setup_failure_after_spawn_confirms_exact_child_reap`
  after 140 other library tests passed.
- The exact failed test immediately passed 1/1 on rerun.
- A second untouched complete baseline passed: 141 library tests and 171
  integration tests, plus the five existing nested compile-fail fixtures.

No production change was made in response to the transient baseline result.

### Initial Task 13 RED

The required command was run after adding the Task 13 test surfaces and before
production implementation:

```text
cargo test --test diagnostics_contract --test end_to_end_native --test cli_end_to_end
```

It failed for the expected missing-feature reasons:

- unresolved `rustedoutclient::diagnostics`;
- missing `StartupCoordinator`;
- missing `execute_headless`, `CliFuture`, and `CliRuntime`;
- missing `AppEvent::PhaseTiming` and `AppEvent::ChildExitStatus`;
- missing `AppState::diagnostic_record` and cleanup telemetry.

The private parser-focused RED command also failed before the private seam was
implemented because `VncClient::run_test_stream` was absent and the ticket
generation counter was inaccessible outside its existing test module.

### Focused startup-policy RED

After the core implementation, a focused regression test was added before its
production helper:

```text
cargo test --lib native_startup_preserves_configured_clipboard_while_honoring_explicit_view_only -- --nocapture
```

It failed with the expected unresolved
`configured_startup_command` import. The implementation then preserved the
configured clipboard boundary while applying the CLI's explicit view-only
value; the exact test passed 1/1.

### Privacy-contract correction during GREEN

A first full run detected that a temporary `Debug` implementation for
`AppCommand` violated the existing compile-fail privacy contract. That
implementation was removed, the new test was written without requiring
`AppCommand: Debug`, and the exact existing trybuild contract passed again.
The final tree therefore retains the Task 10/11 rule that semantic commands
capable of carrying clipboard input are not debuggable.

## Implementation summary

### Typed diagnostics

`DiagnosticRecord` is serialized only from typed allowlisted fields. It cannot
accept raw stderr, environment data, a target, ticket, clipboard text,
framebuffer bytes, an arbitrary map, or prebuilt output. `AppState` retains at
most 16 typed phase timings and an optional numeric child status. The existing
Diagnostics window and Copy Diagnostics action both now call the same typed
record-to-text path; the prior ad hoc builder was removed.

The manager measures phase intervals using monotonic `tokio::time::Instant`
and emits bounded integer-millisecond `PhaseTiming` events before the matching
state transition snapshot. `AppState::apply` remains pure and has no clock or
I/O access.

### CLI orchestration

`CliRuntime` is a small typed service boundary over semantic `AppCommand` and
`AppEvent`. Its only production constructor loads the existing private schema-1
configuration path and calls `SessionManager::spawn_production`; the binary has
no test/backend/endpoint/SSH replacement route.

- `list` ignores cached inventory, waits finitely for live inventory, prints
  only VMID/status/name, and shuts down its exact manager.
- `probe` selects a running VM from live inventory, starts timing at semantic
  native `Open`, waits for the exact session's first non-empty bounded
  framebuffer event, computes checked geometry and RGB-only non-black count,
  discards the event bytes, closes that exact session, and shuts down. Exit zero
  requires both frame evidence and successful cleanup.
- `open` creates one typed startup request. Cached inventory cannot authorize
  it. The app resolves only the first live inventory event and dispatches one
  native action or one explicitly requested TigerVNC action. Native failure
  never changes transport. The native path preserves configured clipboard
  policy and applies the explicit view-only request.
- The normal GUI remains interactive, with 1280x800 only as initial logical
  window geometry. Tests exercise the coordinator without launching a window.

### Deterministic RFB and lifecycle acceptance

The peer runs only over bounded in-memory duplex streams. It performs RFB 3.8,
offers only security type 2, verifies a fixed synthetic authentication vector,
sends bounded 64x64 server initialization, and drives the production parser and
decoder. The generic stream entry is private and compiled only for crate unit
tests; the public `VncClient` entry still requires `TrustedSshProxy`.

Capture retains at most 128 typed client-message facts. Clipboard content and
pixel/authentication payloads are consumed or discarded and never retained in
the capture, report, logs, or files.

The typed manager/backend integration fixture covers cache/live replacement,
per-open live revalidation, duplicate focus, two-session capacity, reconnect,
fresh authorization generations, first frame, input facts, resize policy, and
exact synthetic ownership cleanup. Existing private real-production-wire tests
remain in force, with an added reconnect assertion proving two fresh proxy
ticket generations without exposing either value.

## Diagnostics field matrix

| Field | Shape | Source | Export rule |
| --- | --- | --- | --- |
| `app_version` | string | compile-time package version | always |
| `upstream_base_sha` | string | fixed approved upstream SHA | always |
| `os` | string | compile-time platform constant | always |
| `architecture` | string | compile-time platform constant | always |
| `profile_display_name` | string | validated app configuration state | always |
| `node` | string | typed `NodeName` | always |
| `vmid` | integer | typed `VmId` | optional |
| `phases` | ordered array | typed `PhaseTiming` | always, max 16 |
| `phase` | closed string enum | typed `SessionPhase` | per timing |
| `duration_ms` | bounded `u64` | monotonic duration | per timing |
| `child_exit_status` | signed integer | typed optional status | optional |
| `error_category` | closed string enum | `PublicErrorKind` mapping | optional |
| `cleanup_failed` | boolean | typed public cleanup truth | optional |

Explicitly absent and tested: VM name, SSH target, host fingerprint, path, PID,
runtime/session UUID, wall-clock timestamp, raw error/stderr, ticket/private-key
material, clipboard content, framebuffer bytes, and arbitrary environment data.

`ProbeReport` JSON contains exactly:

```text
vmid
first_frame_ms
frame_width
frame_height
non_black_pixels
result
```

Failures use typed result values and safe zero evidence fields. Alpha-only
pixels remain RGB-black and are not counted.

## CLI behavior and exit matrix

| Command/outcome | Inventory authority | Output | Cleanup | Exit |
| --- | --- | --- | --- | --- |
| no command | GUI live state | interactive GUI | GUI close coordinator | GUI result |
| `list` success | live only | VMID/status/name | manager shutdown | 0 |
| `list` config/live/cleanup failure | never cache-authorized | typed public error only | attempted shutdown when constructed | nonzero |
| `open ... --viewer native` | first live match | GUI | normal GUI lifecycle | GUI result |
| `open ... --viewer tiger-vnc` | first live match | GUI | Task 12 owned lifecycle | GUI result |
| native `probe` success | live running match | exact text/JSON report | exact session close then manager shutdown | 0 |
| `probe` timeout/session failure | live only | safe typed report | exact known session close then shutdown | nonzero |
| `probe` cleanup failure | live only | `cleanup` result | bounded cleanup attempted | nonzero |
| missing/private config | none | content-free `configuration failed` | no manager constructed | nonzero |

Real-binary process tests pass for help, version, missing private config,
content-free failure output, and absence of host/endpoint/password/ticket/
SSH-executable/backend/fixture flags. Successful offline orchestration runs only
through a self-spawned integration-test executable.

## Encoding, input, malformed, and resize matrix

| Area | Case | Evidence |
| --- | --- | --- |
| Handshake | RFB 3.8 | exact version exchange |
| Security | only type 2 | selected and fixed synthetic DES response verified |
| Raw | 1x1 non-black | production decoder framebuffer event |
| CopyRect | Raw seed plus copy | production decoder framebuffer event |
| Hextile | raw tile | production decoder framebuffer event |
| ZRLE | bounded zlib raw tile | production decoder framebuffer event |
| Tight | bounded fill | production decoder framebuffer event |
| SetEncodings | seven exact IDs | bounded typed fact |
| Update | initial 64x64 request | bounded typed fact |
| Pointer | down and zero-button owner release | exact typed facts |
| Key | ordinary down/up and release-all | exact typed facts |
| Ctrl+Alt+Delete | Ctrl down, Alt down, Delete down/up, Alt up, Ctrl up | exact six-message window |
| Clipboard | explicit bounded send | length only; no payload retained |
| Malformed banner | closed enum | finite typed failure and joined peer/client tasks |
| Malformed security list | closed enum | finite typed failure and joined peer/client tasks |
| Malformed rectangle | closed enum | finite typed failure and joined peer/client tasks |
| 1600x900 viewport | normalized 1600x896 | one request, Forwarded=Pending, actual geometry=Applied |
| 1920x1080 viewport | exact 1920x1080 | request exceeds 1280 width; no guest cap |
| Unsupported | typed protocol outcome | local Fit remains available |
| Timeout | one in-flight correlation | typed TimedOut; explicit retry; local Fit available |
| SetDesktopSize wire | 24 bytes | type,pad,w,h,count,pad,id,x,y,w,h,flags |

## Final required gates

All commands below were run on the final implementation tree represented by
`5b299cee096ed43e8b1b4ee04ede079845078511`:

| Gate | Result |
| --- | --- |
| `cargo fmt --all` | PASS |
| `cargo test --test diagnostics_contract --test end_to_end_native --test cli_end_to_end` | PASS: 4 + 4 + 7 = 15 |
| `cargo test --test app_state_contract --test display_resize_contract` | PASS: 13 + 17 = 30 |
| `cargo test --test session_manager_contract` | PASS: 23 |
| `cargo test --all-targets` | PASS: 146 library + 186 integration = 332, plus five nested compile-fail fixtures |
| `cargo clippy --all-targets -- -D warnings` | PASS |
| `cargo build --release` | PASS |
| `git diff --check` | PASS |
| `bash -n tests/support/fake_ssh.sh` | PASS |

### Five independent cleanup runs

Exact command, invoked as five separate processes:

```text
cargo test --test end_to_end_native ten_connect_disconnect_cycles_leave_zero_exact_owned_residue -- --exact
```

| Run | Result | Cycles | Final exact residue |
| --- | --- | --- | --- |
| 1 | PASS 1/1 | 10 | 0 sessions/processes/tasks/listeners/artifacts |
| 2 | PASS 1/1 | 10 | 0 sessions/processes/tasks/listeners/artifacts |
| 3 | PASS 1/1 | 10 | 0 sessions/processes/tasks/listeners/artifacts |
| 4 | PASS 1/1 | 10 | 0 sessions/processes/tasks/listeners/artifacts |
| 5 | PASS 1/1 | 10 | 0 sessions/processes/tasks/listeners/artifacts |

Total independently repeated cycles: 50.

## Dependency delta

The only direct dependency change is the brief-approved dev dependency:

```toml
assert_cmd = "2"
```

The lockfile resolved `assert_cmd 2.2.2` and its normal transitive test-only
graph (`bstr`, `predicates`, `predicates-core`, `predicates-tree`, `difflib`,
`termtree`, and `wait-timeout`; already-present shared crates are reused). No
runtime dependency was added or changed.

## Changed files

Implementation commit (17 files):

- `Cargo.toml`
- `Cargo.lock`
- `src/diagnostics.rs`
- `src/lib.rs`
- `src/cli.rs`
- `src/main.rs`
- `src/app/mod.rs`
- `src/app/state.rs`
- `src/session/events.rs`
- `src/session/model.rs`
- `src/session/manager.rs`
- `src/ssh/proxy.rs` (test-only counter visibility only)
- `src/vnc/client.rs` (private `cfg(test)` acceptance entry/tests plus shared channel packaging)
- `tests/support/rfb_peer.rs`
- `tests/end_to_end_native.rs`
- `tests/diagnostics_contract.rs`
- `tests/cli_end_to_end.rs`

Evidence-only follow-up:

- `.superpowers/sdd/2026-08-29-rustedoutclient-proxmox-vnc/task-13-report.md`

`src/app/view.rs` required no direct diff: its existing Diagnostics window and
Copy Diagnostics action already consume `AppState::diagnostics_summary`, which
now has exactly one typed `DiagnosticRecord::to_text` implementation.

## Public-surface and security self-review

- Production SSH remains `SshCommandFactory::new` with fixed
  `/usr/bin/ssh`, strict known-host options, fixed inventory/proxy commands, and
  no production executable override.
- `ProductionCliRuntime::load` is the only binary runtime construction path and
  loads only the existing private schema-1 configuration/cache locations.
- No production environment variable, endpoint, host, ticket, password,
  backend, fixture, command-string, stream, or executable selector was added.
- Native VNC still publicly accepts only `TrustedSshProxy`. The arbitrary
  stream helper is private, `cfg(test)`, and covered by the existing public API
  compile-fail fixture.
- Native VNC source and Task 13 fixtures contain no TCP listener, bind, or LAN
  socket. The Task 12 loopback fallback boundary is unchanged.
- No diagnostic or probe type accepts raw blobs or performs substring
  redaction. No diagnostic/probe persistence or guest-pixel write path exists.
- New logs are limited to typed `PublicError` display or a fixed GUI failure
  message. Existing RFB debug logs contain only dimensions or fixed event text.
- `AppCommand`, `InputAction`, clipboard-bearing VNC commands/events, and
  clipboard text remain non-`Debug`; the compile-fail privacy contract passes.
- UI and CLI issue only semantic `AppCommand` values. Raw `VncCommand` is not
  exposed to either surface.
- Native failure has no branch to TigerVNC. All fallback actions are explicit
  UI or explicit `ViewerMode::TigerVnc` request paths.
- Active native/fallback capacities, queue sizes, clipboard slot/limits,
  framebuffer limits, close deadlines, input release order, and resize
  correlation were not broadened.
- Tests use only `.invalid` identities, deterministic local memory streams,
  temporary private directories, and exact test-owned resources.

## Protected boundaries

Post-implementation protected metadata/hash verification:

| Artifact | Mode | Size | mtime | SHA-256 |
| --- | ---: | ---: | ---: | --- |
| `/Users/Adam.Gell/.local/bin/pve-vnc` | 700 | 9182 | 1786720979 | `e69819f3650f5e3632bcf7aa1d9bd991d58267ab1559d3964d6bfa947c95dd62` |
| `/Users/Adam.Gell/.config/pve-vnc/config.json` | 600 | 101 | 1786657951 | `f2b7d608f98d4f81fef0327abab3b1e1e8456d2042e5cceeede2ce0c903eaabc` |
| `/Users/Adam.Gell/Desktop/Open PVE VNC.command` | 700 | 468 | 1786658121 | `1ed2561a440ec700cd0f73946256954819035e782b8ac35077678d3f92b5e2fe` |
| `/opt/homebrew/bin/vncviewer` | 755 | 42 | 1786657823 | `ec58c3d51b44040cf9c044f3b2bee5d9cc256ef4975bb0370a66e735fb37e833` |

The protected configuration was hash/stat checked only; its contents were not
read or used.

Parked Task 8 remains exactly:

```text
stash@{0} 0d95b4f403abb215f8d6d9f8e21d64201d60e76a
```

No protected or Task 8 path appears in the implementation diff.

## Residual concerns

- This task is intentionally deterministic offline acceptance. It does not
  claim live Proxmox, macOS/Windows GUI, real host clipboard, or installed
  TigerVNC acceptance; those remain later live-acceptance work.
- Numeric child exit status remains optional and is absent when the lower
  native session layer has no truthful status to report.
- Task 14 CI/dependency-policy/operator-documentation work was not started.
