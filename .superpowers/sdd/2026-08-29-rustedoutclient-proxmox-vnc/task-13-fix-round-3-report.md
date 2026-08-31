# Task 13 fix round 3 report

## Status and exact revisions

- Result: PASS for the deterministic offline scope in
  `task-13-fix-round-3.md`.
- Reviewed base: `5b9e5284ccb9288c4946683af062b1e68aadc1cf`.
- Verified test-code head: `15b2b71638200121dffdf57f6e9f2e6b99a002a8`.
- Test-code commit: `15b2b71638200121dffdf57f6e9f2e6b99a002a8`
  (`test: cover all production listener surfaces`).
- This report is committed as a documentation-only child of the verified
  test-code head. Its SHA is necessarily recorded in the controller/final
  response rather than in its own contents.
- The implementation delta contains only `tests/fallback_contract.rs`.
  Production sources, dependencies, manifests, and the lockfile are unchanged.
- No live Proxmox endpoint or VM, real configuration body, real clipboard, or
  installed TigerVNC viewer was accessed.

## Remaining finding resolved

The listener/bind policy now recursively and deterministically enumerates all
Rust sources below `src`, rather than a hand-selected native subset.

### Complete production-source coverage

- The current tree contains 42 `src/**/*.rs` files; all 42 are sorted and
  inspected on every policy execution.
- Fourteen files currently end in a conventional
  `#[cfg(test)] mod tests { ... }` module. The scanner removes that terminal
  test module before evaluating production text.
- Exclusion is conservative and fail-safe: a file fails the policy if it has
  multiple apparent test modules, a non-exact conventional marker, an
  unterminated module, or any source after the terminal test module.
- Files without a conventional test module are inspected in full. A new Rust
  source added anywhere below `src` is included automatically.

This covers the previously omitted app, CLI, diagnostics, cache, config,
runtime, model, SSH, session, library/binary root, and fallback peer modules.

### Exact relay allowlist

`src/fallback/relay.rs` is the only approved listener/bind source. Its
production portion must contain exactly one literal:

```text
TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
```

The compact production text must also contain exactly one bind call, including
turbofish-aware detection. A second bind, a changed/non-loopback bind, or an
additional socket family fails the contract. The relay may retain its required
`TcpListener` and `TcpStream`; `TcpSocket`, `UdpSocket`, `UnixListener`,
`UnixStream`, and `UnixDatagram` are rejected there.

Every other production Rust source rejects all seven listener/socket surfaces
and every detected bind call. The prior native loopback-endpoint restriction is
also preserved, with the existing explicit TigerVNC viewer endpoint in
`src/fallback/mod.rs` remaining the sole endpoint exception.

The accepted ten-cycle cleanup policy remains unchanged: it still rejects an
unrelated `TcpListener`, listener task, rebind evidence, or test-side
`drop(runtime)`. No runtime/task/PID ownership implementation was modified.

## Tests-first RED evidence

Before replacing the hand-selected scan, the existing policy's represented
paths were made explicit and compared with the independently recursive
`src/**/*.rs` inventory.

Command:

```text
cargo test --test fallback_contract listener_process_and_password_artifacts_stay_inside_the_approved_boundary -- --exact --nocapture
```

Result: FAIL, exit 101, 0 passed / 1 failed. The failure enumerated these 24
uncovered production Rust sources:

```text
src/app/actions.rs
src/app/mod.rs
src/app/state.rs
src/app/view.rs
src/cache.rs
src/cli.rs
src/config.rs
src/diagnostics.rs
src/fallback/mod.rs
src/fallback/password_file.rs
src/fallback/viewer.rs
src/lib.rs
src/main.rs
src/model.rs
src/runtime.rs
src/session/mod.rs
src/session/model.rs
src/ssh/command.rs
src/ssh/error.rs
src/ssh/inventory.rs
src/ssh/master.rs
src/ssh/mod.rs
src/ssh/proxy.rs
src/ssh/stream.rs
```

No prohibited listener was inserted into production to manufacture RED.

## Focused GREEN evidence

| Gate | Result |
|---|---:|
| Full-source listener/bind policy | PASS, 1/1 |
| Complete fallback contract | PASS, 6/6 |
| Corrected manager-owned ten-cycle test, focused run | PASS, 1/1 |
| `cargo test --test diagnostics_contract --test end_to_end_native --test cli_end_to_end` | PASS, 7 + 3 + 12 = 22 |
| `cargo test --test app_state_contract --test display_resize_contract` | PASS, 13 + 17 = 30 |
| `cargo test --test session_manager_contract` | PASS, 23/23 |
| `cargo test --lib vnc::client::tests::` | PASS, 19/19 |
| `cargo test --lib session::manager::tests::` | PASS, 24/24 |

The policy test count is unchanged; the existing test was strengthened.

## Five independent cleanup executions

Exact command, invoked as five separate test processes on the final test-code
tree:

```text
cargo test --lib session::manager::tests::ten_connect_disconnect_cycles_leave_zero_exact_owned_residue -- --exact
```

| Run | Result | Cycles | Exact terminal/residue result |
|---:|---:|---:|---|
| 1 | PASS 1/1 | 10 | exact manager/VNC tasks terminal; all exact fake-SSH PIDs and runtime paths gone |
| 2 | PASS 1/1 | 10 | same assertions passed |
| 3 | PASS 1/1 | 10 | same assertions passed |
| 4 | PASS 1/1 | 10 | same assertions passed |
| 5 | PASS 1/1 | 10 | same assertions passed |

Total independently repeated final-tree coverage: 50 production-wire cycles.

## Complete required gates

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | PASS |
| `cargo test --all-targets --quiet` final rerun | PASS, 344/344 total |
| Library tests | PASS, 151/151 |
| Integration tests | PASS, 193/193 |
| Compile-fail fixtures | PASS, 6/6 |
| `cargo clippy --all-targets -- -D warnings` | PASS, zero warnings |
| `cargo build --release` | PASS |
| `bash -n tests/support/fake_ssh.sh` | PASS |
| `git diff --check` | PASS |
| Manifest/lock comparison to reviewed base | PASS, no delta |

The all-target integration counts remained
`13, 3, 12, 14, 7, 17, 39, 3, 6, 7, 14, 3, 16, 23, 4, 11, 1`, totaling
193.

### Existing synthetic readiness transient

The first all-target attempt passed 150/151 library tests, then the unchanged
`ssh::stream::tests::setup_failure_after_spawn_confirms_exact_child_reap`
fixture timed out at its PID-marker wait after 101.57 seconds. Under full-suite
parallel load, the existing 150 ms setup-failure cleanup path can reap the
synthetic helper before its shell writes the marker. This is the same transient
recorded before Task 13 implementation; neither `src/ssh/stream.rs` nor
`tests/support/fake_ssh.sh` differs from the reviewed base.

The exact failed test then passed six consecutive isolated executions (one
diagnostic run plus five reproductions, each about 0.12-0.13 seconds). Without
any source change, the complete all-target command was rerun and passed all
344 tests. No unrelated SSH timing or lifecycle correction was added in this
narrow source-policy round.

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

## Changed files and self-review

- `tests/fallback_contract.rs`: recursive all-`src` production extraction and
  exact relay-only listener/bind allowlist.
- This report is the only evidence-file addition.

No production Rust source, dependency, fixture, manifest, lockfile, lifecycle
probe, Dynamic Resolution behavior, SSH behavior, VNC behavior, fallback
behavior, endpoint route, backend selector, or public API changed.

This remains deterministic offline synthetic conformance evidence. It does not
claim live Proxmox, guest-driver, real clipboard, real TigerVNC, or VM
acceptance.
