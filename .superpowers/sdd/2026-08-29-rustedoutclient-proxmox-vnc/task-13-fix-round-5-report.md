# Task 13 fix round 5 report

## Status and exact revisions

- Result: PASS for the deterministic offline scope in
  `task-13-fix-round-5.md`.
- Reviewed base: `5f49be2aa8e35207ef7ce312929d682205675317`.
- Verified test-code commit: `3a1061df84657567a563eecf2628b9dcde94ed78`
  (`test: count bind turbofish conservatively`).
- This report is a documentation-only child of the verified test-code commit.
  Its SHA is necessarily recorded by the controller/final response rather than
  in its own contents.
- The implementation commit changes only `tests/fallback_contract.rs`.
- This report is the only other changed file. Production Rust, dependencies,
  manifests, the lockfile, fixtures, the accepted lexer, and protected rollback
  artifacts are unchanged.
- No live Proxmox endpoint or VM, private configuration body, real clipboard,
  or installed TigerVNC viewer was accessed or launched.

## Remaining finding resolved

`bind_invocation_count` no longer tries to interpret the contents of a Rust
turbofish. After excluding a lexical `fn bind` declaration, the exact token
prefix `bind :: <` is now counted immediately as a bind capability. The generic
body is never parsed and can no longer become an inconclusive path that silently
continues without a count.

This is deliberately conservative. A turbofish function-item reference counts
even when no following call parenthesis exists, because the production policy
has no legitimate need to retain a generic bind capability. That fail-closed
false positive is the behavior required by the correction brief.

Direct calls and parenthesized non-turbofish calls retain their prior detection.
Comment/literal/identifier separation, lexical `fn bind` exclusion, the exact
approved relay expression, socket-family restrictions, endpoint cardinality,
matching-brace production extraction, recursive 42-file source coverage, and
all ownership/lifecycle tests are unchanged.

## Tests-first RED evidence

One focused table-driven regression was added before changing the scanner. It
uses seven hand-written expected counts and exercises:

1. const generic shift: `Endpoint<{ 1 << 1 }>`;
2. const generic comparison: `Endpoint<{ 1 > 0 }>`;
3. nested function type: `Endpoint<fn() -> SocketAddr>`;
4. ordinary nested generic closers: `Endpoint<Vec<Vec<u8>>>`;
5. a shift turbofish separated by comments and whitespace;
6. a parenthesized shift-turbofish callee; and
7. a turbofish function-item reference.

Command:

```text
cargo test --test fallback_contract bind_counter_counts_turbofish_capabilities_without_parsing_generic_contents -- --exact
```

Result: FAIL, exit 101; 0 passed / 1 failed / 1 run. The actual assertion was:

```text
left:  [("const generic shift", 0),
        ("const generic comparison", 0),
        ("nested function type", 0),
        ("ordinary nested generic closers", 1),
        ("comment-separated turbofish", 0),
        ("parenthesized turbofish callee", 0),
        ("turbofish function-item reference", 0)]
right: [("const generic shift", 1),
        ("const generic comparison", 1),
        ("nested function type", 1),
        ("ordinary nested generic closers", 1),
        ("comment-separated turbofish", 1),
        ("parenthesized turbofish callee", 1),
        ("turbofish function-item reference", 1)]
```

The failure proves that six relevant turbofish forms were missed while the one
ordinary balanced generic happened to count. No prohibited construct was added
to production source to manufacture RED.

## Focused GREEN evidence

| Gate | Result |
|---|---:|
| New seven-case turbofish regression | PASS, 1/1 |
| Complete fallback contract | PASS, 15/15 |
| Corrected manager-owned ten-cycle test, standalone focused run | PASS, 1/1 |
| `diagnostics_contract` + `end_to_end_native` + `cli_end_to_end` | PASS, 7 + 3 + 12 = 22 |
| `app_state_contract` + `display_resize_contract` | PASS, 13 + 17 = 30 |
| `session_manager_contract` | PASS, 23/23 |
| `vnc::client::tests::` | PASS, 19/19 |
| `session::manager::tests::` | PASS, 24/24 |

## Independent ownership repetitions

The exact command below was invoked once as the focused ownership gate and then
five more times as five independent test processes:

```text
cargo test --lib session::manager::tests::ten_connect_disconnect_cycles_leave_zero_exact_owned_residue -- --exact
```

| Process | Result | Cycles |
|---:|---:|---:|
| Focused gate | PASS, 1/1 | 10 |
| Independent run 1 | PASS, 1/1 | 10 |
| Independent run 2 | PASS, 1/1 | 10 |
| Independent run 3 | PASS, 1/1 | 10 |
| Independent run 4 | PASS, 1/1 | 10 |
| Independent run 5 | PASS, 1/1 | 10 |

Required standalone ownership coverage therefore completed 60 production-wire
connect/disconnect cycles with exact manager/VNC task terminal evidence, exact
fake-SSH PID disappearance, and exact runtime-path cleanup.

## Complete required gates

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | PASS |
| `cargo test --all-targets --quiet` | PASS, 353/353 total |
| Library tests | PASS, 151/151 |
| Integration tests | PASS, 202/202 |
| New test delta from reviewed base | exactly +1, in `fallback_contract` |
| Compile-fail fixtures | PASS, 6/6 |
| `cargo clippy --all-targets -- -D warnings` | PASS, zero warnings |
| `cargo build --release` | PASS |
| `bash -n tests/support/fake_ssh.sh` | PASS |
| `git diff --check` | PASS |
| Production/manifest/lock comparison to reviewed base | PASS, no delta |
| Production Rust source count | 42 recursively enumerated files |

The all-target integration counts were
`13, 3, 12, 14, 7, 17, 39, 3, 15, 7, 14, 3, 16, 23, 4, 11, 1`, totaling
202. The all-target run completed without the previously documented synthetic
SSH PID-marker transient.

## Protected artifacts and parked stash

Only metadata and SHA-256 were read for the private configuration file. Its
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

- `tests/fallback_contract.rs`: one seven-case regression and the conservative
  immediate count for lexical `bind :: <`.
- `.superpowers/sdd/2026-08-29-rustedoutclient-proxmox-vnc/task-13-fix-round-5-report.md`:
  this evidence report only.

Self-review confirmed that the correction removes generic-body interpretation
rather than expanding it. No lexer, production source, public/test production
API, dependency, manifest, lockfile, fixture, lifecycle probe, Dynamic
Resolution behavior, SSH behavior, VNC behavior, fallback behavior, endpoint
policy, or protected artifact changed.

This remains deterministic offline synthetic conformance evidence. It does not
claim live Proxmox, guest-driver, real clipboard, real TigerVNC, or VM
acceptance.
