# Task 13 fix round 2 report

## Status and exact revisions

- Result: PASS for the deterministic offline scope in `task-13-fix-round-2.md`.
- Reviewed base: `6d0529a58548ec5651e25971da830833d9f5f2f0`.
- Verified implementation head: `57f3d180b191a79a6977035dd0215144fe200a01`.
- Implementation commit: `57f3d180b191a79a6977035dd0215144fe200a01`
  (`fix: tie cleanup evidence to manager lifecycle`).
- This report is committed as a documentation-only child of the verified
  implementation head. Its SHA is necessarily recorded in the controller/final
  response rather than in its own contents.
- Dependency delta: none. `Cargo.toml` and `Cargo.lock` are unchanged from the
  reviewed base.
- No live Proxmox endpoint or VM, real configuration body, real clipboard, or
  installed TigerVNC viewer was accessed.

## Remaining finding resolved

The ten-cycle production-wire cleanup regression now observes resources owned
by the real manager lifecycle rather than independent test sentinels.

### Runtime ownership

The private `ProductionWireBackend` now directly owns its `RuntimeDir`, in the
same manager-worker lifecycle position as `ProductionBackend`. The runtime is
moved into the backend before `SessionManager` is spawned. The test retains
only the exact runtime directory path needed for its post-shutdown assertion;
its temporary control-socket path is discarded before close. There is no
test-side `drop(runtime)`.

Immediately before its owned `RuntimeDir` field is dropped, the private backend
captures the exact fake-SSH PIDs already recorded under the fixture marker.
`SessionManager::shutdown` then joins the worker, consumes/drops the backend,
and the test proves the exact runtime path no longer exists. Every captured PID,
including the opened proxy PID, is checked individually with PID identity
probing until it is gone. No process-name scan is used.

### Exact actual-task terminal evidence

Private `#[cfg(test)]` task probes allocate monotonically unique identities and
carry started, dropped, and joined state:

- The manager probe guard is installed inside the actual worker future passed
  to the manager's `tokio::spawn`. Its joined state is set only after the actual
  worker `JoinHandle` resolves in `SessionManager::shutdown`.
- The VNC probe guard is installed inside the actual native VNC future passed to
  `tokio::spawn`. Its joined state is set only after the actual VNC
  `JoinHandle` resolves in `ProductionSession::close`, including the bounded
  abort-and-await path.
- Every cycle asserts both exact identities started, were joined, and dropped.
  The ten manager identities and ten VNC identities are unique and disjoint.

The non-test manager spawn remains the direct original
`tokio::spawn(worker_state.run())` shape. Probe types, fields, constructors, and
join bookkeeping are all compiled only under `#[cfg(test)]`; no public or
production test hook was added.

### Listener boundary

The unrelated standalone listener, listener task, stop channel, and rebind
assertion were removed from the cleanup regression. The existing fallback
boundary contract now recursively scans native VNC sources and checks the
production section of the session manager for listener/socket types and bind
calls. It also pins the only approved listener to the explicit TigerVNC relay's
exact loopback bind. The contract rejects listener evidence or a manual runtime
drop if either is reintroduced into the ten-cycle cleanup proof.

## Tests-first RED evidence

Two focused failures were captured before implementation.

### Unrelated listener RED

Command:

```text
cargo test --test fallback_contract listener_process_and_password_artifacts_stay_inside_the_approved_boundary -- --exact --nocapture
```

Result: FAIL, exit 101, 0 passed / 1 failed. The exact failure was:

```text
cleanup proof retained unrelated listener evidence: TcpListener
```

### Ownership and actual-task identity compile RED

After expressing the required manager-owned runtime and task observations in
the ten-cycle regression, this command was run before implementation:

```text
cargo test --lib ten_connect_disconnect_cycles_leave_zero_exact_owned_residue --no-run
```

Result: compile FAIL, exit 101. The relevant compiler failures were:

- `E0609`: no field `runtime` on `ProductionWireBackend`;
- `E0433`: undeclared `TaskTerminalProbe` for both actual task identities;
- `E0425`: missing `production_owned_wire_manager` lifecycle constructor.

The remaining type-inference errors were direct cascades from the missing
constructor. No production leak was introduced to manufacture runtime RED.

## Focused GREEN evidence

| Gate | Result |
|---|---:|
| Listener/bind ownership source policy | PASS, 1/1 |
| Exact manager-owned ten-cycle regression | PASS, 1/1 |
| `cargo test --test diagnostics_contract --test end_to_end_native --test cli_end_to_end` | PASS, 7 + 3 + 12 = 22 |
| `cargo test --test app_state_contract --test display_resize_contract` | PASS, 13 + 17 = 30 |
| `cargo test --test session_manager_contract` | PASS, 23/23 |
| `cargo test --test fallback_contract` | PASS, 6/6 |
| `cargo test --lib vnc::client::tests::` | PASS, 19/19 |
| `cargo test --lib session::manager::tests::` | PASS, 24/24 |
| `cargo check` | PASS |

The accepted Task 1-12 and Task 13 round-1 behavior remains covered, including
strict SSH transport, bounded parser/decoder behavior, 24-byte
SetDesktopSize encoding, resize outcomes, semantic app commands, typed cleanup
truth, clipboard privacy, and explicit-only TigerVNC fallback.

## Five independent cleanup executions

Exact command, invoked as five separate test processes:

```text
cargo test --lib session::manager::tests::ten_connect_disconnect_cycles_leave_zero_exact_owned_residue -- --exact
```

| Run | Result | Cycles | Exact terminal/residue result |
|---:|---:|---:|---|
| 1 | PASS 1/1 | 10 | 10 exact manager tasks and 10 exact VNC tasks terminal; all exact fake-SSH PIDs and runtime paths gone |
| 2 | PASS 1/1 | 10 | same assertions passed |
| 3 | PASS 1/1 | 10 | same assertions passed |
| 4 | PASS 1/1 | 10 | same assertions passed |
| 5 | PASS 1/1 | 10 | same assertions passed |

Total independently repeated final-tree coverage: 50 production-wire cycles,
50 manager-task identities, and 50 VNC-task identities.

## Complete required gates

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | PASS |
| `cargo test --all-targets --quiet` | PASS, 344/344 total |
| Library tests | PASS, 151/151 |
| Integration tests | PASS, 193/193 |
| Compile-fail fixtures | PASS, 6/6 |
| `cargo clippy --all-targets -- -D warnings` | PASS, zero warnings |
| `cargo build --release` | PASS |
| `bash -n tests/support/fake_ssh.sh` | PASS |
| `git diff --check` | PASS |
| Manifest/lock comparison to reviewed base | PASS, no delta |

The all-target integration counts were
`13, 3, 12, 14, 7, 17, 39, 3, 6, 7, 14, 3, 16, 23, 4, 11, 1`, totaling
193. The six compile-fail fixtures remained the cleanup mutator, untrusted
stream, pixel conversion, raw input, clipboard debug, and legacy split cleanup
contracts.

## Protected artifacts and parked stash

Only metadata and SHA-256 were read for the private configuration file; its
contents were not printed or inspected.

| Protected artifact | Mode | Size | mtime | SHA-256 | Result |
|---|---:|---:|---:|---|---|
| `/Users/Adam.Gell/.local/bin/pve-vnc` | 700 | 9182 | 1786720979 | `e69819f3650f5e3632bcf7aa1d9bd991d58267ab1559d3964d6bfa947c95dd62` | unchanged |
| `/Users/Adam.Gell/.config/pve-vnc/config.json` | 600 | 101 | 1786657951 | `f2b7d608f98d4f81fef0327abab3b1e1e8456d2042e5cceeede2ce0c903eaabc` | unchanged |
| `/Users/Adam.Gell/Desktop/Open PVE VNC.command` | 700 | 468 | 1786658121 | `1ed2561a440ec700cd0f73946256954819035e782b8ac35077678d3f92b5e2fe` | unchanged |
| `/opt/homebrew/bin/vncviewer` | 755 | 42 | 1786657823 | `ec58c3d51b44040cf9c044f3b2bee5d9cc256ef4975bb0370a66e735fb37e833` | unchanged |

- Parked Task 8 remains exactly `stash@{0}` /
  `0d95b4f403abb215f8d6d9f8e21d64201d60e76a`
  (`task8-parser-qa-pending-program-access`).
- The stash was listed by identity only. It was not shown, applied, dropped,
  edited, or recreated.

## Changed files

- `src/session/manager.rs`: private test-only exact task terminal guards,
  manager-owned production-wire runtime/PID evidence, and the corrected
  ten-cycle lifecycle regression.
- `tests/fallback_contract.rs`: recursive native listener/bind source boundary
  and removal-policy contract for the cleanup regression.

No dependency, fixture script, production endpoint, backend selector, SSH
executable route, raw stream constructor, listener hook, or fallback behavior
was added.

## Self-review and remaining limitations

- The production manager and VNC paths retain their verified transport and
  cleanup behavior; only private test builds receive identity probes.
- Runtime disappearance is caused by manager-owned backend destruction after
  the actual worker join. The test never manually destroys a runtime owner.
- PID evidence is exact and fixture-recorded. No broad process lookup or
  process-name inference is used.
- Native production remains listener-free. The explicit TigerVNC relay remains
  loopback-only and unchanged; no viewer was launched.
- No real configuration content, clipboard text, credential, ticket, guest
  pixel payload, private endpoint, or live infrastructure was accessed or
  retained.
- This is deterministic offline synthetic conformance evidence only. It does
  not claim live Proxmox, guest-driver, real clipboard, real TigerVNC, or VM
  acceptance.
