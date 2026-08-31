# Task 14A Hosted-CI Source-Matcher Fix Report

## Status

The hosted-CI source-matcher bootstrap failure is repaired locally with strict
RED/GREEN evidence. The workflow now installs exact locked ripgrep 15.2.0
before either matcher-dependent policy block, and an executable contract starts
without `rg`, runs the real install block, then runs the real source-policy
block against a synthetic clean source tree.

This report does not claim hosted CI is green. The remediation was not pushed,
the draft PR was not read or modified, and no fresh hosted run has observed the
new commit. Independent review and a controller-owned push/run remain required.

## Starting identity and boundaries

- Shared linked worktree:
  `/Users/Adam.Gell/repo/worktrees/RustedOutClient-feature-proxmox-console`
- Branch: `feature/proxmox-console-foundation`
- Required and observed clean starting HEAD:
  `1f8384adfd0c254b346db09f0d9b9915efd7c8d7`
- Required and observed starting tree:
  `228bd54e37704c7d1122c881e3d1ecc86bba0365`
- Configured upstream: `origin/feature/proxmox-console-foundation`
- Observed upstream commit:
  `1f8384adfd0c254b346db09f0d9b9915efd7c8d7`
- Observed `origin`: `https://github.com/adamgell/RustedOutClient.git`
- Starting tracked and untracked status: clean
- Superproject: none

No stash command was run. The parked Task 8/14B stash and every parser/fuzz
target, corpus, script, workflow, input, smoke test, and ignored fuzz directory
were neither inspected nor modified. No other worktree was changed. No push,
amend, merge, rebase, branch operation, PR operation, or live acceptance was
performed.

## Hosted failure and established root cause

The failed GitHub Actions evidence was:

- run: `33401426994`;
- job: `99518395823`;
- exact head: `1f8384adfd0c254b346db09f0d9b9915efd7c8d7`;
- runner: official `macos-26` ARM64 job.

Checkout, architecture assertion, pinned Rust setup, pinned
`cargo-audit`/`cargo-deny` installation, formatting, all 356 tests, and
warnings-denied Clippy passed. `Enforce source policy` then failed closed with
exact content-free output:

```text
Required source-policy matcher is unavailable
```

The preflight was correct; the bootstrap assumption was not. The official
runner-image inventory did not list ripgrep, and the workflow never installed
it. Prior local verification had not reproduced this because the developer Mac
already had ripgrep 15.2.0.

## Strict RED/GREEN evidence

The test was added before the workflow changed:

```text
ci_bootstraps_exact_pinned_source_matcher_before_source_policy
```

It extracts the real pinned-tool `run: |` block and the real source-policy
`run: |` block. The execution environment is test-owned and cleared; its
initial `PATH` contains one specific fake `cargo` and no `rg`. The fake Cargo
boundary enforces this exact order:

```text
install cargo-audit --version 0.22.2 --locked
install cargo-deny --version 0.19.0 --locked
install ripgrep --version 15.2.0 --locked
```

Only the exact third invocation can create an executable clean matcher. The
synthetic matcher returns status 1 for both real policy queries over a
synthetic clean `src` tree. Deleting, misspelling, moving, reordering,
unpinning, or unlocking the ripgrep command therefore makes the executable
contract fail.

Actual RED against the untouched workflow:

- process exit: 101;
- test result: 0 passed, 1 failed, 4 filtered;
- synthetic stdout:
  `Required source-policy matcher is unavailable\n`;
- synthetic stderr: empty.

The minimum workflow change was exactly one line in the existing pinned tool
block:

```bash
cargo install ripgrep --version 15.2.0 --locked
```

Actual GREEN:

- focused bootstrap contract: 1 passed, 0 failed;
- complete `surface_policy` suite: 5 passed, 0 failed.

The existing source-policy contract remained green for missing matcher,
status-0 matches, status-1 absence, independent status-greater-than-1 errors,
partial-output suppression, and stderr suppression. The existing advisory
guard contract remained green for missing/error/active/absent matcher results,
graph generation failure, ordering, and cleanup.

## Implementation and compatibility

The implementation commit is:

- `6d9444d4e1597dccbad5265b3023150454cc4858`
- `fix: install pinned CI source matcher`
- tree `2f9ca31d36c4aab95d6df199f3152b8b41d3e737`

It changes exactly:

- `.github/workflows/ci.yml`
- `tests/surface_policy.rs`

Ripgrep 15.2.0 is exact and locked. It requires Rust 1.85 and is therefore
compatible with the already-selected exact Rust 1.92.0 toolchain. Its declared
license is `Unlicense OR MIT`. No dependency manifest or lockfile changed, and
no Homebrew, curl/download logic, cache, artifact, mutable latest install,
secret, permission, or unrelated workflow behavior was added.

## Threat-model citations

The added workflow line changes the final workflow length from 128 to 129.
Every workflow citation in `docs/threat-model.md` was updated and checked
against the control its claim names:

| Semantic range | Final range | Citations | Validated content |
| --- | --- | ---: | --- |
| complete macOS job | `16-129` | 3 | runner through locked release build, including pinned matcher install |
| complete source policy | `53-89` | 2 | matcher preflight and both independent three-way result handlers |
| supported graph through exact audit | `91-123` | 2 | matcher preflight, locked graph, three-way result, exact two-ID audit |
| exact cargo-deny step | `125-126` | 1 | named step and exact aggregate command |

Recalculated results:

- 192/192 threat-model citations point to existing valid file ranges;
- 112 unique path/range tuples;
- 8/8 workflow citation semantics pass; and
- 24/24 local Markdown links resolve.

The citation changes do not alter any threat, mitigation, residual-risk, or
acceptance claim.

## Offline verification

The full pre-evidence-commit sequence passed on the final workflow, test, and
documentation content:

| Gate | Result |
| --- | --- |
| focused new workflow bootstrap test | 1 passed; 0 failed |
| all `surface_policy` tests | 5 passed; 0 failed |
| `cargo fmt --all -- --check` | Pass |
| `cargo test --all-targets --all-features --locked` | 357 passed; 0 failed; 0 ignored |
| `cargo clippy --all-targets --all-features --locked -- -D warnings` | Pass |
| `cargo build --release --locked` | Pass |
| `actionlint .github/workflows/ci.yml` | Pass |
| actual extracted source-policy block | Pass; clean absence on both matchers |
| actual extracted supported-target graph block | Pass; `quick-xml` absent; test-owned temporary directory empty |
| exact `cargo audit --ignore RUSTSEC-2026-0194 --ignore RUSTSEC-2026-0195` | Pass; 346 dependencies, 0 vulnerabilities, 1 allowed `ttf-parser` maintenance warning |
| `cargo deny list` | Pass |
| `cargo deny check advisories --warn unmaintained` | Pass; reviewed `ttf-parser` warning visible |
| `cargo deny check licenses` | Pass; required unused MPL-2.0 warning visible |
| `cargo deny check bans` | Pass |
| `cargo deny check sources` | Pass |
| `cargo deny check --warn unmaintained` | Pass |
| every threat-model citation | 192/192 valid; 112 unique |
| workflow citation semantics | 8/8 valid |
| local Markdown links | 24/24 valid |

The 357-test total is:

```text
151 + 0 + 13 + 3 + 12 + 14 + 7 + 17 + 39 + 3 + 15 + 7 + 14 + 3 +
16 + 23 + 4 + 11 + 5 = 357
```

The documentation/report commit uses exact message
`docs: record hosted CI matcher remediation`. Because it contains this report,
its SHA and the repeated final-HEAD gate outcome are returned in the concise
completion response instead of being embedded self-referentially.

## Protected artifacts and privacy

Only metadata and SHA-256 were checked for the four protected rollback
artifacts; no content was read or printed. The starting values matched the
required baseline exactly:

| Generic path | Mode | Bytes | mtime | SHA-256 |
| --- | ---: | ---: | ---: | --- |
| `~/.local/bin/pve-vnc` | 700 | 9182 | 1786720979 | `e69819f3650f5e3632bcf7aa1d9bd991d58267ab1559d3964d6bfa947c95dd62` |
| `~/.config/pve-vnc/config.json` | 600 | 101 | 1786657951 | `f2b7d608f98d4f81fef0327abab3b1e1e8456d2042e5cceeede2ce0c903eaabc` |
| `~/Desktop/Open PVE VNC.command` | 700 | 468 | 1786658121 | `1ed2561a440ec700cd0f73946256954819035e782b8ac35077678d3f92b5e2fe` |
| `/opt/homebrew/bin/vncviewer` | 755 | 42 | 1786657823 | `ec58c3d51b44040cf9c044f3b2bee5d9cc256ef4975bb0370a66e735fb37e833` |

The same metadata/hash-only comparison is repeated at final HEAD. No private
configuration content, credential, host/address, fingerprint, pixel,
clipboard value, raw environment snapshot, or private stderr was accessed.

## Scope and remaining concerns

- Task 8 and Task 14B parser/fuzz work were not run, inspected, changed, or
  recreated. Fuzzing remains deferred.
- No live Proxmox, VM, native acceptance, real clipboard, real TigerVNC,
  fallback acceptance, protected rollback route, rollback action, or rollout
  was contacted or executed.
- No Rust production code, dependency, manifest, lockfile, security policy,
  native recipe, fallback behavior, or transport/parser lifecycle changed.
- The first hosted run failed closed at the pre-fix head. The local regression
  proves the exact bootstrap contract, but only a fresh controller-owned hosted
  run after independent review can establish hosted green status.
- `ttf-parser` 0.25.1 remains the existing reviewed unmaintained dependency,
  and `quick-xml` 0.39.4 remains the existing unsupported-target lockfile-only
  advisory exception. This focused fix does not change either decision.
