# Task 13 fix round 1 report

## Status and exact revisions

- Result: PASS for the offline synthetic scope in `task-13-fix-round-1.md`.
- Reviewed base: `3084ebaa179fbcd0dd01bde3a674a1ef65ed7ed4`.
- Verified implementation head: `d70e211e3c9cb4ba554c10323ebc1d981b492172`.
- Implementation commit: `d70e211e3c9cb4ba554c10323ebc1d981b492172` (`fix: close Task 13 conformance gaps`).
- The report is committed as a documentation-only child of the verified implementation head; its SHA is necessarily recorded in the controller/final response rather than in its own contents.
- Dependency delta: none. `Cargo.toml` and `Cargo.lock` are byte-identical to the reviewed base.
- No live Proxmox, VM, real configuration body, real clipboard, or installed TigerVNC viewer was accessed.

## Review findings resolved

| Finding | Correction and regression evidence |
|---|---|
| Distinct final pixel truth | `ProbeReport::from_frame` now allocates one bounded non-pixel state byte per validated framebuffer coordinate, applies rectangles in order with checked last-write-wins indexing, counts final RGB-nonblack coordinates once, and consumes the derived state. Tests cover overlapping nonblack writes, later black overwrite, partial rectangles, out-of-bounds geometry, and `u32` addition overflow. |
| Timeout and channel closure | `probe --timeout-seconds` accepts only `1..=300`, retaining default `30`. Every deadline uses checked construction. Closed inventory/probe channels fail immediately with typed queue/cleanup results and still execute shutdown or exact-session close plus shutdown. Direct `u64::MAX` runtime construction is typed instead of panicking. |
| View-only presence | CLI absence is represented as `None`; `--view-only` produces `Some(true)` and `--view-only=false` is rejected. Native and explicit TigerVNC startup preserve global/per-favorite policy when absent and force true when present. Startup remains a semantic `StartupAction`; UI code never receives raw VNC commands. |
| List cleanup truth | Operation and shutdown results are explicitly composed for all four combinations. A simultaneous primary and cleanup failure retains the primary kind and sets cleanup-failure truth. |
| Behavioral repeated cleanup | Inert `exact_owned_*` counters and their integration test were removed. A private crate-unit production-wire test performs ten real fake-SSH/native connect-close cycles, records every exact top-level fake-SSH PID, awaits exact manager and listener tasks, proves the exact loopback test address can be rebound, captures every exact runtime artifact path, and proves all PIDs/paths are gone after every cycle. |
| Rejected resize lifecycle | The private peer now emits distinct RFB results for Apply (`0`), Reject (`1`), and Unsupported (`3`). Parser tests assert each typed outcome. Manager/AppState tests prove Rejected, Unsupported, and TimedOut preserve `Ready` connectivity and local Fit; successful 1600x900 backing pixels normalize and apply as 1600x896. |
| Exact wire/encoding assertions | Raw, CopyRect, Hextile, ZRLE, and Tight now assert exact destination geometry and RGBA. CopyRect specifically proves `(0,0)` dirty output covering `2x1` with the copied destination pixel. The capture asserts every update-request field and every fixed SetDesktopSize field. Production SetDesktopSize remains exactly 24 bytes in the required order `type,pad,w,h,count,pad,id,x,y,w,h,flags`. |
| Cleanup API boundary | `PublicError::with_cleanup_failure` is restored to `pub(crate)`. Internal diagnostics exercise cleanup truth; a new compile-fail fixture proves external callers cannot invoke the mutator. |

## Tests-first RED evidence

The following failures were captured before the corresponding corrections:

| Finding | Focused RED command/result |
|---|---|
| Pixel overlap | `cargo test --test diagnostics_contract probe_counts_distinct_final_pixels_with_last_write_wins -- --exact` failed with observed `2`, expected `1`. |
| Public mutator | `cargo test --test diagnostics_contract cleanup_failure_mutator_is_not_public_api -- --exact` failed because the compile-fail fixture unexpectedly compiled. |
| Timeout parser | `cargo test --test cli_end_to_end probe_timeout_parser_accepts_only_the_approved_finite_range -- --exact` failed because out-of-range input was accepted. |
| Closed channels | `cargo test --test cli_end_to_end closed_ -- --nocapture` failed both cases at the 250 ms outer bound because production slept toward the 30-second deadline. |
| Checked deadline | `cargo test --test cli_end_to_end direct_runtime_timeout_overflow_is_typed_instead_of_panicking -- --exact` failed with `overflow when adding duration to instant`. |
| List composition | `cargo test --test cli_end_to_end list_composes_primary_and_cleanup_results_without_losing_truth -- --exact` failed because `has_cleanup_failure()` was false for simultaneous failures. |
| View-only absence | `cargo test --lib native_startup_preserves_configured_clipboard_and_absent_view_only_policy` failed because configured true became false. |
| Behavioral cleanup | `cargo test --lib ten_connect_disconnect_cycles_leave_zero_exact_owned_residue -- --nocapture` failed because no exact child-identity record existed. |
| Unsupported resize | `cargo test --lib private_peer_distinguishes_applied_rejected_and_unsupported_resize_replies` failed with observed `Rejected`, expected `Unsupported`. |

The exact wire assertions were coverage hardening where production was already correct. The added exact encoding test also exposed and corrected one test-helper expectation for Tight RGB ordering; no production decoder workaround was introduced.

## Focused GREEN evidence

| Gate | Result |
|---|---:|
| `cargo test --test cli_end_to_end` | PASS, 12/12 |
| `cargo test --test diagnostics_contract` | PASS, 7/7 including the new compile-fail fixture |
| `cargo test --test end_to_end_native` | PASS, 3/3 |
| `cargo test --test app_state_contract` | PASS, 13/13 |
| `cargo test --test display_resize_contract` | PASS, 17/17 |
| `cargo test --test session_manager_contract` | PASS, 23/23 |
| `cargo test --lib vnc::client::tests::` | PASS, 19/19 |
| `cargo test --lib session::manager::tests::` | PASS, 24/24 |
| Checked-deadline unit regression | PASS, 1/1 |
| Startup global/favorite native/Tiger regression | PASS, 1/1 |
| Rejected/unsupported/timed-out manager/AppState regression | PASS, 1/1 |

## Five independent cleanup executions

Exact command:

```text
cargo test --lib session::manager::tests::ten_connect_disconnect_cycles_leave_zero_exact_owned_residue -- --exact
```

| Run | Test result | Cycles | Exact residue result |
|---:|---:|---:|---|
| 1 | PASS 1/1 | 10 | all recorded fake-SSH PIDs gone; manager/listener tasks joined; exact listener address reusable; all exact runtime paths gone |
| 2 | PASS 1/1 | 10 | same zero-residue assertions |
| 3 | PASS 1/1 | 10 | same zero-residue assertions |
| 4 | PASS 1/1 | 10 | same zero-residue assertions |
| 5 | PASS 1/1 | 10 | same zero-residue assertions |

This is 50 verified production-wire cycles. The loopback listener is a private test-owned lifecycle sentinel; native production remains listener-free. The private PID recorder is activated only by an exact test-owned runtime marker in the copied fake-SSH fixture and does not alter production SSH construction.

## Complete required gates

| Gate | Result |
|---|---|
| `cargo fmt --all -- --check` | PASS |
| `cargo test --all-targets --quiet` | PASS, 344/344 total tests; 151 library tests plus 193 integration tests |
| Compile-fail coverage | PASS, 6/6 fixtures: cleanup mutator, untrusted stream, pixel conversion, raw input, clipboard debug, legacy split cleanup |
| `cargo clippy --all-targets -- -D warnings` | PASS, zero warnings |
| `cargo build --release` | PASS |
| `git diff --check` | PASS |
| Dependency comparison to reviewed base | PASS, no manifest or lockfile delta |

All-target integration counts were: `13, 3, 12, 14, 7, 17, 39, 3, 6, 7, 14, 3, 16, 23, 4, 11, 1` across the integration targets, totaling 193. The application binary target contained zero unit tests as expected.

## Protected artifacts and parked stash

Only metadata and SHA-256 were read for the private configuration file; its contents were not printed or inspected.

| Protected artifact | Mode | Size | mtime | SHA-256 | Result |
|---|---:|---:|---:|---|---|
| `/Users/Adam.Gell/.local/bin/pve-vnc` | 700 | 9182 | 1786720979 | `e69819f3650f5e3632bcf7aa1d9bd991d58267ab1559d3964d6bfa947c95dd62` | unchanged |
| `/Users/Adam.Gell/.config/pve-vnc/config.json` | 600 | 101 | 1786657951 | `f2b7d608f98d4f81fef0327abab3b1e1e8456d2042e5cceeede2ce0c903eaabc` | unchanged |
| `/Users/Adam.Gell/Desktop/Open PVE VNC.command` | 700 | 468 | 1786658121 | `1ed2561a440ec700cd0f73946256954819035e782b8ac35077678d3f92b5e2fe` | unchanged |
| `/opt/homebrew/bin/vncviewer` | 755 | 42 | 1786657823 | `ec58c3d51b44040cf9c044f3b2bee5d9cc256ef4975bb0370a66e735fb37e833` | unchanged |

- Parked Task 8 remains exactly `stash@{0}` / `0d95b4f403abb215f8d6d9f8e21d64201d60e76a` (`task8-parser-qa-pending-program-access`).
- The stash was listed by identity only; it was not shown, applied, dropped, edited, or recreated.

## Changed files

- `src/diagnostics.rs`: bounded last-write framebuffer truth and internal cleanup diagnostic regression.
- `src/cli.rs`: finite parser range, presence-aware view-only, checked deadlines, immediate channel-close handling, and explicit operation/cleanup composition.
- `src/app/mod.rs`: semantic startup action and configuration-preserving native/Tiger resolution.
- `src/session/model.rs`: crate-private cleanup mutator.
- `src/session/manager.rs`: private ten-cycle behavioral production-wire cleanup test.
- `src/vnc/client.rs`: exact encoding/wire assertions and parameterized resize outcome peer test.
- `tests/cli_contract.rs`: view-only presence and false-value rejection contract.
- `tests/cli_end_to_end.rs`: parser, deadline, closure, cleanup composition, and startup regressions.
- `tests/diagnostics_contract.rs`: overlap/overwrite/partial/invalid geometry and public-boundary tests.
- `tests/end_to_end_native.rs`: rejected lifecycle coverage and removal of inert cleanup counters.
- `tests/support/fake_ssh.sh`: exact private test-owned PID recording under a runtime marker.
- `tests/support/rfb_peer.rs`: complete typed wire capture and correct Unsupported result.
- `tests/ui/public_cleanup_mutator.rs` and `.stderr`: compile-fail API boundary.

## Self-review and remaining limitations

- The chosen approved finite probe range is 1 through 300 seconds, preserving the useful 30-second default and matching the existing bounded configuration horizon.
- SetDesktopSize encoding is still exactly 24 bytes; no 26-byte fixture compatibility was introduced.
- Native production source remains strict-SSH/`TrustedSshProxy` only and does not bind a TCP listener. The only new listener calls are inside the private `#[cfg(test)]` cleanup regression.
- No backend, SSH executable, endpoint, credential, environment, or raw-stream override was added to production.
- Explicit TigerVNC startup remains explicit; no automatic fallback path was added and no viewer was launched during verification.
- Diagnostics retain typed allowlisted fields only; no guest pixels, clipboard text, raw stderr, process identity, private path, target, ticket, or configuration content is persisted or rendered.
- This is deterministic offline synthetic conformance evidence only. It does not claim live Proxmox, guest-driver, real clipboard, real TigerVNC, or VM acceptance.
