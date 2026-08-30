# Task 12 implementation report

## Scope and exact base

- Task: explicit loopback-only TigerVNC fallback.
- Required base: `69302781959d89988665e1036bc003458c334fcd`.
- Implementation commit: `35099161c46a1462429c49d177c50c400c358f48` (`feat: retain loopback TigerVNC fallback`).
- Branch/worktree: `feature/proxmox-console-foundation` in the controller-provided linked worktree.
- No amend, push, merge, rebase, reset, stash operation, subagent, live Proxmox/VM access, real clipboard/viewer access, dependency installation, or Task 13 orchestration occurred.

Before editing, HEAD was exactly the required base, the tracked tree was clean, and parked Task 8 remained:

```text
stash@{0} 0d95b4f403abb215f8d6d9f8e21d64201d60e76a On feature/proxmox-console-foundation: task8-parser-qa-pending-program-access
```

Baseline focused gates were green before RED:

- `cargo test --test app_state_contract`: 12 passed.
- `cargo test --test session_manager_contract`: 23 passed.

## RED evidence

Tests were added before fallback production code.

`cargo test --test fallback_contract` failed as required:

```text
error[E0432]: unresolved import `rustedoutclient::fallback`
could not find `fallback` in `rustedoutclient`
error: could not compile `rustedoutclient` (test "fallback_contract")
```

The focused app action test also failed before the semantic command existed:

```text
cargo test --test app_state_contract explicit_fallback_requires_configuration_and_targets_tab_before_running_inventory -- --exact
error[E0599]: no variant named `OpenInTigerVnc` found for enum `AppCommand`
error: could not compile `rustedoutclient` (test "app_state_contract")
```

These failures established both missing production boundaries rather than relying on an assertion altered to match existing behavior.

## Implemented behavior

### Password file

- Added the historical one-block TigerVNC DES transform using the fixed key bytes required by the brief.
- The canonical synthetic eight-byte fixture matches its exact deterministic eight-byte result.
- A unique file is created only inside the existing `RuntimeDir`; mode 0600 is set at creation and reasserted before any bytes are written while the containing runtime remains mode 0700.
- Only the obfuscated block is written. The clear block and in-memory ciphertext block are explicitly zeroized.
- The consumed `ProxyTicket` is dropped immediately after the private file is created and before the obfuscated bytes are written.
- Explicit removal is idempotent. Drop retries ownership cleanup. Removal failures are typed and render no path or content.

### Viewer validation and launch

- Validation rejects relative, missing, broken-link, directory, and non-executable paths.
- Metadata follows the final symlink, so an absolute symlink to a regular executable is accepted.
- The exact configured path is executed directly through Tokio with no shell or PATH lookup.
- Fixed arguments are exactly `-Shared=1`, `-RemoteResize=1`, `-SecurityTypes=VncAuth`, `-PasswordFile`, the private file path, requested optional flags, and `127.0.0.1::<port>`.
- `-FullScreen=1` and `-ViewOnly=1` are each included exactly once only when selected.
- Standard input/output/error are null, no unbounded child output is captured, and `LC_PVE_TICKET` is explicitly removed from the child environment.

### Relay and lifecycle

- Production binds only `TcpListener::bind((Ipv4Addr::LOCALHOST, 0))` inside `src/fallback/relay.rs`.
- The production accept deadline is exactly 20 seconds.
- One IPv4 loopback peer is accepted, verified as loopback, and the listener is dropped before password removal or relay work. A later connection to the exact owned port is refused.
- The password file is removed immediately after acceptance and before byte relay.
- Relay uses Tokio `copy_bidirectional`, retaining bounded library buffers rather than an application-owned unbounded queue.
- `FallbackSession` owns cancellation, owner task, terminal result, and one absolute three-second production close budget. Viewer termination/reap and proxy close run under the same deadline.
- Explicit close is finite and idempotent. Drop signals cancellation; owner-task cancellation drops the exact listener/socket/child/proxy/file authorities.
- Only the exact Tokio viewer child is killed/reaped. The exact `ProxyStream` lifecycle owns its SSH child; no process-name scan or broad termination exists.

### Manager and app

- Added one semantic `AppCommand::OpenInTigerVnc` containing only VMID and `FallbackPreferences`.
- Production keeps the viewer path in `ProductionBackend`; pure `AppState` stores only `fallback_configured: bool`.
- The action is enabled only when fallback is configured and either a selected native tab or a selected running inventory item supplies the target.
- Selected tab/inventory view-only preference and current fullscreen preference are preserved in the semantic command.
- Existing input-owner cleanup is retained ahead of fallback on the same bounded FIFO app-command channel; queue pressure blocks fallback until cleanup is accepted.
- The worker permits at most two distinct fallback VMIDs, rejects duplicate/third sessions with a typed content-free error, polls completion nonblockingly, and releases capacity after completion.
- Production validates the configured viewer path, verifies the master, then uses `TrustedSshProxy::connect`; that existing boundary rechecks live inventory before generating the fresh ticket and spawning the fixed proxy.
- Native failures do not enqueue fallback.
- Shutdown closes fallback sessions before native-session teardown, before SSH master close, and therefore before worker event-channel completion permits final app-window close.
- No Task 13 CLI/process-level orchestration or production SSH/viewer override was added.

## Cleanup and fault matrix

| Case | Focused evidence | Final owned state |
|---|---|---|
| Password explicit close and Drop | deterministic password-file unit tests | file absent; repeated removal succeeds |
| Password removal fault | one-shot private fault seam | typed redacted error; Drop/cleanup retry removes file |
| Viewer path rejection | relative/missing/broken/directory/non-executable table | no listener or viewer; exact proxy close attempted |
| Viewer spawn failure | executable fixture with missing interpreter | listener dropped, proxy closed, password removed, no owner task |
| Viewer exits before connect | test-owned executable exits immediately | listener/socket absent, viewer reaped, proxy closed, file removed |
| Accept timeout | short private policy corresponding to exact production 20-second branch | viewer killed/reaped, listener absent, proxy closed, file removed |
| One-client relay/EOF | exact owned port plus 64-byte duplex peer | bidirectional bytes pass; second connect refused; all resources gone |
| Relay error | private failing async stream | typed relay primary retained; child/file/listener/task gone |
| Explicit cancellation/repeated close | pending accept plus two close calls | one cleanup, same result, exact port refused |
| Session Drop/task cancellation | dropped session with pending accept | cancellation completes, exact PID gone, no password/owner task |
| Natural viewer exit | accepted client plus test-controlled natural process exit | success completion and complete resource cleanup |
| Real owned proxy | verified fake SSH master/proxy plus test-owned viewer | exact viewer and proxy PIDs reaped; one-client port refused |
| Duplicate/third admission | manager fake with three owned sessions | duplicate and third rejected; completion releases one slot |
| Native failure | manager native-open failure | one native typed error; fallback open count unchanged |
| Manager/app shutdown | close flags asserted inside backend master-close operation | every fallback close completes before master close/event completion |

No test invoked the installed TigerVNC binary. All process fixtures were private test-owned executable files; all network use was local in-process IPv4 loopback; all profile data used `.invalid` names.

## Final GREEN verification

The complete required sequence ran in order on the final implementation tree and exited 0:

```text
cargo fmt --all
cargo test --test fallback_contract
cargo test --test app_state_contract
cargo test --test session_manager_contract
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo build --release
git diff --check
```

Exact results:

- fallback contract: 6 passed, 0 failed;
- app-state contract: 13 passed, 0 failed;
- session-manager contract: 23 passed, 0 failed;
- library: 133 passed, 0 failed;
- integration: 171 passed, 0 failed;
- all-target conventional total: 304 passed, 0 failed;
- nested compile-fail fixtures: 5 passed;
- formatting: pass;
- Clippy with `-D warnings`: pass;
- release build: pass;
- diff whitespace check: pass.

Additional focused private tests passed:

- fallback module/password/relay: 9 passed;
- fallback admission/native-failure/shutdown ordering: 1 passed;
- app input-owner/fallback FIFO ordering: 1 passed.

## Listener, process, and source audits

- Exact-owned-port tests prove the listener is IPv4 loopback, the second connection is refused after first acceptance, and the same port is refused after cancellation cleanup.
- A post-gate read-only `lsof` filter found no RustedOutClient, synthetic viewer, or fake-SSH listener. It emitted only an unrelated Time Machine SMB metadata warning.
- Post-gate exact-process checks found no synthetic viewer, fake-SSH, or RustedOutClient test process.
- A post-gate `/tmp` scan found no `.vnc-password-*.bin` file.
- Production `TcpListener` appears only in `src/fallback/relay.rs`; native VNC/session event code remains listener-free.
- Production viewer spawning appears only in `src/fallback/mod.rs`; it uses the exact configured path.
- The TigerVNC fixed DES password-file key and ticket exposure appear only in `src/fallback/password_file.rs` outside their pre-existing native VNC-auth and strict SSH transport boundaries.
- `LC_PVE_TICKET` in fallback production appears only in `env_remove`; other fallback matches are synthetic tests.
- No fallback production logging, tracing, diagnostic capture, environment dump, direct VNC endpoint, bind-address parameter, shell command, or raw `VncCommand` UI surface exists.

## Public-surface and privacy review

- `TigerVncFallback::open` consumes exactly `TrustedSshProxy`, `&RuntimeDir`, `&Path`, and `FallbackPreferences`, returning owned `FallbackSession`.
- Generic stream, timeout, listener observer, process behavior, and removal fault seams are private/test-only.
- Public fallback errors retain only an enum category and cleanup-failure bit. Display and Debug contain no path, clear ticket, ciphertext, argv, stderr, endpoint, or environment data.
- App state/diagnostics contain only fallback configuration presence; the configured executable path remains at the config/production-backend boundary.
- Viewer argv contains the private file path but neither clear ticket nor ciphertext. Child environment receives no `LC_PVE_TICKET`.
- No clear ticket, ciphertext, config contents, environment dump, clipboard value, guest data, or raw child output entered test output or this report.

## Dependency delta

None. `Cargo.toml` and `Cargo.lock` are byte-for-byte unchanged by Task 12. Existing `des`, `cipher`, `zeroize`, `tempfile`, UUID, and Tokio dependencies were sufficient.

## Exact changed files

Implementation commit: 13 files, 2,397 insertions, 9 deletions.

- `src/fallback/mod.rs` (new)
- `src/fallback/password_file.rs` (new)
- `src/fallback/relay.rs` (new)
- `tests/fallback_contract.rs` (new)
- `src/lib.rs`
- `src/session/events.rs`
- `src/session/manager.rs`
- `src/session/mod.rs`
- `src/app/actions.rs`
- `src/app/state.rs`
- `src/app/view.rs`
- `src/app/mod.rs` (focused owner-order test only)
- `tests/app_state_contract.rs`

`src/main.rs`, `src/cli.rs`, CLI end-to-end tests, Cargo manifests/lockfiles, Task 8 files/stash, and protected rollback artifacts are absent from the implementation diff.

## Protected rollback artifacts after gates and implementation commit

All values match the pre-edit baseline exactly:

```text
/Users/Adam.Gell/.local/bin/pve-vnc
mode=0700 size=9182 mtime=1786720979
sha256=e69819f3650f5e3632bcf7aa1d9bd991d58267ab1559d3964d6bfa947c95dd62

/Users/Adam.Gell/.config/pve-vnc/config.json
mode=0600 size=101 mtime=1786657951
sha256=f2b7d608f98d4f81fef0327abab3b1e1e8456d2042e5cceeede2ce0c903eaabc

/Users/Adam.Gell/Desktop/Open PVE VNC.command
mode=0700 size=468 mtime=1786658121
sha256=1ed2561a440ec700cd0f73946256954819035e782b8ac35077678d3f92b5e2fe

/opt/homebrew/bin/vncviewer
type=Symbolic Link mode=0755 size=42 mtime=1786657823
sha256=ec58c3d51b44040cf9c044f3b2bee5d9cc256ef4975bb0370a66e735fb37e833
```

The installed viewer was never executed.

## Self-review and residual concerns

Self-review found and corrected three lifecycle edge cases before the final gates: listener ownership is now dropped for every accept outcome, a viewer wait error is not falsely treated as a confirmed reap, and the `ProxyTicket` is dropped immediately after private file creation rather than after the subsequent write/sync.

No unresolved implementation concern remains within Task 12. By explicit boundary, live Proxmox/TigerVNC acceptance is not claimed here and remains a later acceptance task. Task 13 CLI orchestration is intentionally absent.

# Task 12 fix round 1

## Exact fix base and implementation commits

- Fix base: `915d55283315ff19a8b4b7c413bb315529665e63`.
- Primary fix implementation commit: `4d285733e6553ab2db960de9df42705606c7aad2` (`fix: pin and revoke fallback resources`).
- Single-open integration commit: `d00bdfaf8ff66cfa8553667ed45325f85c31335a` (`fix: preserve single-open viewer launch`).
- The tracked tree was clean at the exact fix base before editing.
- No amend, push, merge, rebase, reset, stash operation, subagent, dependency change, live Proxmox/VM access, installed-viewer execution, real clipboard access, private-configuration content inspection, Task 13 work, or protected-artifact mutation occurred.

## Fix-round-1 RED evidence

Every production change followed a focused failing regression.

### Important 1 — Drop revoked the exact owner task

`cargo test --lib fallback::tests::drop_aborts_stalled_owner_and_only_its_exact_resources -- --exact --nocapture` failed before the Drop fix:

```text
fallback owner task remained alive: Elapsed(())
test result: FAILED. 0 passed; 1 failed
```

The test used a test-owned proxy child whose cooperative shutdown remained pending for 60 seconds. It separately observed that the owner future had entered, then required owner termination, exact-port refusal, exact viewer/proxy PID disappearance, password cleanup, and survival of an unrelated exact PID. This proved the defect was detached task ownership rather than missing cancellation delivery.

### Important 2 — viewer bytes were not pinned and size was unbounded

Before snapshot implementation, the over-64-MiB opened-file test failed because the executable was accepted:

```text
called `Result::unwrap_err()` on an `Ok` value: ()
test result: FAILED. 0 passed; 1 failed
```

The complete snapshot regressions then failed to compile because the safe boundary did not exist:

```text
unresolved import `super::viewer`
could not find `viewer` in `super`
struct `OpenPolicy` has no field named `viewer_snapshot`
no variant or associated item named `ViewerSnapshot`
```

Those tests were already asserting opened-FD symlink replacement resistance, mode-0500 private snapshots, minimal child environment, and create/write/sync cleanup before production support was added.

### Important 3 — password write/sync failures lost cleanup evidence

The password fault tests first failed to compile because no owner-retaining constructor policy existed:

```text
unresolved imports `PasswordFileFault`, `PasswordFilePolicy`
no function or associated item named `create_with_policy`
no variant or associated item named `Sync`
```

After the private constructor lifecycle existed, the production-conversion regression separately failed before wiring because `OpenPolicy` had no `password_file` fault boundary. This prevented a false GREEN that tested only an internal error while omitting `FallbackError::has_cleanup_failure` propagation.

### Self-review — manager must not reopen the configured viewer path

After all three findings were implemented, a source-policy regression exposed that the production manager still performed preliminary path validation before the secure snapshot boundary opened the configured path again:

```text
cargo test --test fallback_contract production_manager_rechecks_inventory_before_one_pinned_viewer_open -- --exact --nocapture
assertion failed: !fallback.contains("validate_viewer_path")
test result: FAILED. 0 passed; 1 failed
```

The manager-side reopen and its production-visible helper were removed. The final production order is live master verification, fresh trusted proxy creation, then one `TigerVncFallback::open` operation that opens, validates, and snapshots the configured viewer through one descriptor before listener/viewer launch.

### Final-gate test isolation corrections

An initial all-target gate exposed three test-only races: process-global environment mutation affected concurrent tests, and two artifact readers accepted a just-created zero-length file. Replacing global environment mutation with a command-local test seed and requiring non-empty output fixed those races. A subsequent run then exposed one intentionally empty fake-SSH state marker; separating existence polling from non-empty output polling fixed that final test-only race. The fresh final all-target run below passed 312 conventional tests plus all five compile-fail fixtures.

## Implemented corrections

### Exact owner-task revocation

- Exceptional `FallbackSession::Drop` still sends cooperative cancellation, then calls `abort()` on the exact owned `JoinHandle` before releasing it.
- Drop remains nonblocking and has no process scan, global kill, second cleanup authority, or fresh timeout budget.
- Explicit `close()` remains the graceful joined/reaped path with its original single absolute three-second budget and unchanged repeated-close result.
- Test owner instrumentation now increments only after the owner future actually enters; termination evidence can no longer be satisfied by a pre-spawn counter.

### FD-pinned viewer snapshot and minimal environment

- Added `src/fallback/viewer.rs` as a private RAII boundary.
- The absolute configured path is opened once for each secure snapshot operation. Regular-file, executable-bit, and 64-MiB limits come from metadata on that opened descriptor.
- Copy reads only that opened descriptor. A deterministic hook replaces the configured symlink after validation; the original opened bytes still execute.
- Snapshot creation uses a unique `create_new` file inside the existing mode-0700 `RuntimeDir`, establishes mode 0500, bounds copied bytes to at most 64 MiB even if the source grows, and requires `sync_all` before launch.
- The snapshot path is launched directly with no shell, PATH lookup of the viewer, configured-path reopen, `canonicalize`, `/dev/fd`, FFI, or unsafe code.
- `OwnedViewer` keeps the kill-on-drop child before the snapshot guard in field-drop order and retains the executable through normal viewer cleanup. Spawn failures return the guard for explicit typed cleanup; owner-task abort drops the exact child and guard.
- Viewer environment starts with `env_clear`. It receives literal `PATH=/usr/bin:/bin` plus only parent `HOME`, `TMPDIR`, `LANG`, `LC_ALL`, and `LC_CTYPE` when present. Synthetic arbitrary, loader, SSH, and ticket variables were all absent in the child.
- The manager has no preliminary viewer-path validation or reopen; the descriptor-backed snapshot boundary is the sole production open of the configured pathname.
- Absolute Homebrew-style symlinks and test-owned absolute-interpreter scripts remain accepted. Relative, missing, broken, directory, non-executable, and over-64-MiB sources fail before listener or viewer launch.

### Password-file owner retention

- `VncPasswordFile` is constructed immediately after private `create_new`, before permission confirmation, write, or sync.
- The `ProxyTicket` is dropped immediately after that owner exists and before any file write.
- Clear and encrypted eight-byte blocks are zeroized on success and every injected write/sync failure; focused observers saw only `[0; 8]` for both blocks.
- Partial and exact write failures preserve `PasswordFileStage::Write`; sync failure preserves `PasswordFileStage::Sync` internally. Public mapping remains the content-free `FallbackErrorKind::PasswordFile`.
- Write/sync failure first removes through the guard. A failed attempt stores the guard inside the redacted typed error, records cleanup failure, permits an explicit retry, and retries again through RAII Drop without exposing the path or bytes.
- `open_with_parts` preserves the primary password-file failure, performs the retained-owner retry, and propagates truth that any removal attempt failed through `FallbackError::has_cleanup_failure`, even when a later retry succeeds.

## Fix-round cleanup and fault matrix

| Case | Focused proof | Final state |
|---|---|---|
| Stalled cooperative Drop | 60-second pending proxy shutdown plus owner-entry/termination observer | owner aborted; exact listener/viewer/proxy gone; unrelated PID alive |
| Symlink replaced after FD validation | configured link swapped to different executable before copy | original opened bytes executed; replacement bytes did not |
| Snapshot create/write/sync | private stage faults, including first removal failure | typed redacted primary; cleanup bit truthful; no snapshot |
| Viewer spawn failure | copied script with missing absolute interpreter | no listener, viewer, proxy, password, snapshot, or owner task |
| Explicit cancellation/repeated close | pending accept, two `close()` calls | exact port refused; password/snapshot removed; identical close result |
| Aborting Drop | stalled owned proxy and exact test processes | child guards invoked; all runtime artifacts removed |
| Viewer exit/timeout/relay/natural exit | existing lifecycle matrix rerun with snapshot assertions | snapshot retained through viewer cleanup then removed |
| Partial write failures | failure after 3 and after all 8 ciphertext bytes | write primary; successful removal; both blocks zeroized |
| Sync plus first removal failure | sync fault and one removal fault | cleanup bit set; error retained owner; RAII retry removed file |
| Repeated removal failure | two consecutive synthetic failures | typed/redacted error retained owner across retry; Drop removed file |
| Production password mapping | write fault plus first removal failure through `open_with_parts` | `PasswordFile` primary plus `has_cleanup_failure=true`; all artifacts gone |

## Final GREEN evidence and exact counts

The focused fallback gate and required final sequence ran on the formatted implementation tree and every command exited 0:

```text
cargo test --lib fallback
cargo fmt --all
cargo test --test fallback_contract
cargo test --test app_state_contract
cargo test --test session_manager_contract
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo build --release
git diff --check
```

Exact results:

- focused fallback contract: 6 passed, 0 failed;
- focused app-state contract: 13 passed, 0 failed;
- focused session-manager contract: 23 passed, 0 failed;
- fallback-related library filter: 23 passed, 0 failed;
- library all-target run: 141 passed, 0 failed;
- integration all-target run: 171 passed, 0 failed;
- conventional all-target total: 312 passed, 0 failed;
- nested compile-fail fixtures: 5 passed;
- strict Clippy: 0 warnings/errors;
- release build: exit 0;
- formatting and whitespace checks: pass.

## Source, privacy, listener, and dependency audits

- `Cargo.toml` and `Cargo.lock` are unchanged; dependency delta is zero.
- Production `TcpListener` remains confined to `src/fallback/relay.rs` and binds exactly IPv4 `127.0.0.1:0`. Native VNC/session-event sources contain neither `TcpListener` nor a direct fallback endpoint.
- Production process launch remains confined to fallback and calls `Command::new(snapshot.path())` after the FD-only copy. The configured path is never used as the launch path.
- No fallback production shell, `/dev/fd`, unsafe code, path-based copy, environment dump, raw child output, or production executable override exists.
- Post-gate process, listener, and `/tmp` checks emitted no synthetic viewer, fake SSH, RustedOutClient test process, `.vnc-password-*.bin`, or `.vnc-viewer-*` result.
- Tests executed only private synthetic scripts and loopback sockets. The installed `/opt/homebrew/bin/vncviewer` was not executed.
- Public/Debug surfaces contain no configured path, raw I/O error, ticket, ciphertext, environment value, VM identity, clipboard content, or endpoint.

## Exact fix-round changed files

Implementation commits relative to the exact fix base: 6 files, 1,201 insertions, 122 deletions.

- `src/fallback/mod.rs`
- `src/fallback/password_file.rs`
- `src/fallback/relay.rs`
- `src/fallback/viewer.rs` (new)
- `src/session/manager.rs`
- `tests/fallback_contract.rs`

The evidence-only follow-up changes `.superpowers/sdd/2026-08-29-rustedoutclient-proxmox-vnc/task-12-report.md` (this report).

Task 13 files, Cargo manifests/lockfiles, Task 8 files/stash, app-state product surfaces, and protected rollback artifacts are absent from the implementation diff. The sole manager change removes its preliminary configured-path validation so the snapshot boundary performs the one production open.

## Parked stash and rollback artifacts after fix gates

The parked Task 8 stash remains exactly:

```text
stash@{0} 0d95b4f403abb215f8d6d9f8e21d64201d60e76a On feature/proxmox-console-foundation: task8-parser-qa-pending-program-access
```

All protected values match the Task 12 pre-fix baseline:

```text
/Users/Adam.Gell/.local/bin/pve-vnc
mode=0700 size=9182 mtime=1786720979
sha256=e69819f3650f5e3632bcf7aa1d9bd991d58267ab1559d3964d6bfa947c95dd62

/Users/Adam.Gell/.config/pve-vnc/config.json
mode=0600 size=101 mtime=1786657951
sha256=f2b7d608f98d4f81fef0327abab3b1e1e8456d2042e5cceeede2ce0c903eaabc

/Users/Adam.Gell/Desktop/Open PVE VNC.command
mode=0700 size=468 mtime=1786658121
sha256=1ed2561a440ec700cd0f73946256954819035e782b8ac35077678d3f92b5e2fe

/opt/homebrew/bin/vncviewer
type=Symbolic Link mode=0755 size=42 mtime=1786657823
sha256=ec58c3d51b44040cf9c044f3b2bee5d9cc256ef4975bb0370a66e735fb37e833
```

## Fix-round self-review and residual concerns

- Important 1: exact owner abort is present; termination and exact-resource cleanup are separately observed; no unrelated process authority was added.
- Important 2: validation/copy/launch are tied to one opened source descriptor with no manager-side preliminary reopen; environment is fixed/minimal; size, mode, transaction, cleanup, symlink, script-interpreter, and all requested fault paths are covered.
- Important 3: cleanup ownership starts at file creation, survives write/sync failure, retries explicitly and through RAII, propagates any cleanup failure, and retains zeroization/early-ticket-drop behavior.
- Original explicit-only fallback, loopback-only listener, trusted SSH transport, two-fallback admission, two-native-session independence, shutdown ordering, pure app-state surface, default native Dynamic Resolution, and Task 13 boundary remain unchanged.

No unresolved code concern remains in Task 12 fix round 1. By explicit scope, this report does not claim live Proxmox, real TigerVNC, or guest acceptance.
