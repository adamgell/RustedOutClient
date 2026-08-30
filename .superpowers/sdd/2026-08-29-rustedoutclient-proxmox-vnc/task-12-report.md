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
