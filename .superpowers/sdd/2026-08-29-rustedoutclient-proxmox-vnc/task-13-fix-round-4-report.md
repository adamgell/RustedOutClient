# Task 13 fix round 4 report

## Status and exact revisions

- Result: PASS for the deterministic offline scope in
  `task-13-fix-round-4.md`.
- Reviewed base: `fa104b4b2e6cc763932725c5c3e15a99516fac94`.
- Verified test-code commit: `1fc452690ad3e4aa03585e3811784573ff4b8f90`
  (`test: close listener policy bypasses`).
- This report is a documentation-only child of that verified test-code commit.
  Its own SHA is necessarily recorded by the controller/final response rather
  than in its contents.
- The implementation commit changes only `tests/fallback_contract.rs`.
- This report is the only other changed file. Production Rust, dependencies,
  manifests, the lockfile, fixtures, and protected rollback artifacts are
  unchanged.
- No live Proxmox endpoint or VM, private configuration body, real clipboard,
  or installed TigerVNC viewer was accessed or launched.

## Reviewer findings resolved

### 1. Brace-aware terminal test-module boundary

`production_portion` now tokenizes Rust source with a deterministic local
lexical scanner before removing a conventional terminal test module. The
scanner handles:

- line comments and nested block comments;
- cooked, byte, character, and byte-character literals with escapes;
- raw and raw-byte strings with arbitrary hash delimiters;
- identifiers, raw identifiers, punctuation, and nested brace depth.

The scanner locates the exact opening brace associated with the one literal
`\n#[cfg(test)]\nmod tests {` marker and walks to its matching closing brace.
Braces inside comments or literals do not affect depth. The suffix is scanned
again and must contain zero lexical tokens, so only whitespace/comment trivia
may follow the matching close. Any production token after `} // tests` is
therefore visible and rejected.

The policy remains fail closed for repeated `mod tests {` declarations,
unsupported formatting, an exact marker that does not match the lexical
module, malformed/unterminated lexical constructs, an unterminated module, or
a nonterminal module.

Pure cases cover ordinary terminal modules, nested code braces, a commented
terminal close, nested comments, cooked/byte/character/raw literals containing
braces, unterminated modules, nonterminal modules, repeated modules,
unsupported layouts, and the exact reviewed `} // tests` hiding mutation.

### 2. Lexical bind-call and exact relay-expression policy

Bind discovery now operates on tokens rather than whitespace-compacted source.
Comments are trivia, literal contents never create calls, and identifiers on
opposite sides of a comment remain separate tokens. The scanner recognizes a
`bind` call across whitespace, line comments, block comments, nested block
comments, optional turbofish syntax, parenthesized callees, and parenthesized
arguments. Function definitions named `bind`, longer identifiers, comments,
and all supported literal forms do not count as invocations.

For `src/fallback/relay.rs`, the policy requires both:

1. exactly one lexical bind invocation; and
2. exactly one literal production expression
   `TcpListener::bind((Ipv4Addr::LOCALHOST, 0))`.

The literal pin is also tied to code tokens. An exact approved string placed in
a comment or string literal cannot satisfy it. A changed/non-loopback call, a
comment-separated second call, or another socket family fails. Every other
production Rust source requires zero bind invocations and zero approved socket
families.

### 3. Exact viewer-endpoint cardinality

The recursive source policy now requires exactly one lexical production
occurrence of `127.0.0.1` in `src/fallback/mod.rs` and zero in every other Rust
source. It separately proves that the approved endpoint file was enumerated.
The focused two-occurrence mutation is rejected.

The existing recursive inventory remains deterministic and currently covers
all 42 `src/**/*.rs` files.

## Tests-first RED evidence

The three mandatory regressions were added against the uncorrected helpers on
the exact reviewed base behavior. No prohibited construct was added to
production source.

Command:

```text
cargo test --test fallback_contract --quiet
```

Result: FAIL, exit 101; 6 passed / 3 failed / 9 total.

Actual failures:

```text
production_portion_rejects_commented_test_close_followed_by_production
  production source after a commented test-module close was hidden

bind_counter_detects_comment_separated_second_relay_bind
  assertion left == right failed
  left: 1
  right: 2
  comment trivia must not hide a second bind invocation

viewer_endpoint_policy_rejects_a_second_approved_file_occurrence
  the approved viewer file must contain exactly one endpoint occurrence
```

The expanded pre-correction lexical matrix then failed 6 of 13 tests. In
addition to the three mandatory failures, it showed that line-comment calls
were missed, braces in terminal module comments/literals were mishandled, and
eight comment/literal/definition decoys were incorrectly counted as binds.

During self-review, the literal approved-bind pin was extracted without a
behavior change and pressure-tested separately:

```text
cargo test --test fallback_contract approved_bind_pin_ignores_exact_decoys_in_comments_and_literals -- --exact
```

Result: FAIL, exit 101; 0 passed / 1 failed. The uncorrected count was 4 rather
than the required 1 because exact decoys in a line comment, block comment, and
string literal were accepted. The token-tied pin corrected that fail-open path.

## Focused GREEN evidence

| Gate | Result |
|---|---:|
| Complete fallback contract, including eight new pure policy tests | PASS, 14/14 |
| Corrected manager-owned ten-cycle test, standalone focused run | PASS, 1/1 |
| `diagnostics_contract` + `end_to_end_native` + `cli_end_to_end` | PASS, 7 + 3 + 12 = 22 |
| `app_state_contract` + `display_resize_contract` | PASS, 13 + 17 = 30 |
| `session_manager_contract` | PASS, 23/23 |
| `vnc::client::tests::` | PASS, 19/19 |
| `session::manager::tests::` | PASS, 24/24 |

## Independent ownership repetitions

The exact command below was invoked once as the focused ownership gate and
then five more times as five independent test processes:

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
| `cargo test --all-targets --quiet` | PASS, 352/352 total |
| Library tests | PASS, 151/151 |
| Integration tests | PASS, 201/201 |
| New test delta from reviewed base | exactly +8, all in `fallback_contract` |
| Compile-fail fixtures | PASS, 6/6 |
| `cargo clippy --all-targets -- -D warnings` | PASS, zero warnings |
| `cargo build --release` | PASS |
| `bash -n tests/support/fake_ssh.sh` | PASS |
| `git diff --check` | PASS |
| Manifest/lock comparison to reviewed base | PASS, no delta |
| Production Rust source count | 42 recursively enumerated files |

The all-target integration counts were
`13, 3, 12, 14, 7, 17, 39, 3, 14, 7, 14, 3, 16, 23, 4, 11, 1`, totaling
201. The all-target run completed without the previously documented synthetic
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

- `tests/fallback_contract.rs`: deterministic lexical policy, matching-brace
  extraction, exact relay bind enforcement, exact viewer endpoint cardinality,
  and pure mutation regressions.
- `.superpowers/sdd/2026-08-29-rustedoutclient-proxmox-vnc/task-13-fix-round-4-report.md`:
  this evidence report only.

Self-review confirmed that no production source, public/test production API,
dependency, manifest, lockfile, lifecycle probe, Dynamic Resolution behavior,
SSH behavior, VNC behavior, fallback behavior, fixture, or protected artifact
changed. The policy scanner is intentionally a narrow deterministic lexer, not
a general Rust parser; unfamiliar or malformed layouts fail closed rather than
being guessed through.

This remains deterministic offline synthetic conformance evidence. It does not
claim live Proxmox, guest-driver, real clipboard, real TigerVNC, or VM
acceptance.
