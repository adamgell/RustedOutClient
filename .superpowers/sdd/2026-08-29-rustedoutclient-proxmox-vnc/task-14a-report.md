# Task 14A Implementation Report

## Outcome

The Task 14A non-fuzz governance/documentation tranche, review-fix rounds 1
through 4, and the controller's post-round-2 cleanup-proof correction are
implemented locally. Independent round 4 returned `Identity: PASS`,
`Spec: FAIL`, and `Quality: CHANGES REQUIRED`, with no Critical or Important
findings and one Minor proxy-ticket-lifetime documentation finding. The
round-4 finding is remediated by the exact documentation and evidence below.
Independent round-5 review is pending and is not claimed as approved. Hosted
CI, live native acceptance, TigerVNC fallback acceptance, rollback execution,
and rollout were not performed and are not claimed.

Task 14B remains the sole deferred parser-smoke/fuzz target, script, corpus, and
workflow scope. No tracked Task 14B target, script, workflow, corpus, or parser
input was introduced or run. The two pre-existing ignored empty directories
under `fuzz/artifacts` remained untouched and contain zero files.

## Exact heads and commits

- Required clean base:
  `d1d5313ef37a6b69c9161965cf32d154828e2e8c`
- Dependency policy/CI commit:
  `6f4e935241181bc1439900a0e41656e03a638be9`
  (`chore: harden RustedOutClient dependency policy`)
- Original implementation/documentation head:
  `6300c977eb092b455c15bf49fe97efce335bf1d6`
  (`docs: define RustedOutClient security and acceptance`)
- Original evidence report and exact review-fix base:
  `73ffcef5940488117ca7367855f183294e35076f`
  (`docs: record Task 14A evidence`)
- Round-1 remediation implementation head:
  `5ff4be0c9db3855c06e3bc9c379dabaa392b6670`
  (`fix: close Task 14A review findings`)
- Round-1 evidence report and exact round-2 fix base:
  `1996cf7fceb4bf314625c83758e967d3d9f775e2`
  (`docs: record Task 14A review round 1 fixes`)
- Round-2 remediation implementation head:
  `48bb0cc1ee52ed237183652282c9404cfdee4102`
  (`fix: fail closed on dependency matcher errors`)
- Initial round-2 evidence report:
  `e0937b1790fbe8b088916539158f6ef169c25ae5`
  (`docs: record Task 14A review round 2 fixes`)
- Native cleanup-proof correction:
  `0200cd8b618ed2d117f3d82c52aa9c3fd4a5e4c7`
  (`fix: verify native dependency graph cleanup`)
- Cleanup-proof evidence report and exact round-3 fix base:
  `6c1be76c726128ae13db7d4e2c27d3ae31720e74`
  (`docs: correct Task 14A round 2 cleanup evidence`)
- Round-3 remediation implementation head:
  `70fef1d6aabf373c2d96f0c83b8c88129f636501`
  (`fix: fail closed on source policy errors`)
- Round-3 evidence report and exact round-4 fix base:
  `b6294e34ac9681c7808aacf9f02f926f917d3734`
  (`docs: record Task 14A review round 3 fixes`)
- Round-4 documentation correction head:
  `5527325f9dfcbd21e9c5e50087f01cae2e0ad6b7`
  (`docs: correct proxy ticket lifetime`)
- This updated report is committed separately. Its new commit SHA and the
  required post-report-commit gate results are reported by the controller-facing
  completion response rather than embedded self-referentially here.

The named branch and shared worktree are preserved. No push, amend, rebase,
merge, or worktree cleanup was performed.

## Changed-file boundary

Commit `6f4e935241181bc1439900a0e41656e03a638be9` contains only:

- `.github/workflows/ci.yml`
- `Cargo.lock`
- `Cargo.toml`
- `deny.toml`

Commit `6300c977eb092b455c15bf49fe97efce335bf1d6` contains only:

- `NOTICE`
- `README.md`
- `SECURITY.md`
- `docs/configuration.md`
- `docs/migration.md`
- `docs/native-acceptance.md`
- `docs/threat-model.md`
- `docs/upstream.md`

The original third commit contains only this report. Round-1 remediation commit
`5ff4be0c9db3855c06e3bc9c379dabaa392b6670` contains exactly:

- `.github/workflows/ci.yml`
- `README.md`
- `SECURITY.md`
- `deny.toml`
- `docs/configuration.md`
- `docs/native-acceptance.md`
- `docs/threat-model.md`
- `src/ssh/command.rs`
- `tests/ssh_command_contract.rs`
- `tests/surface_policy.rs`

The review boundary expanded to production SSH construction and two existing
contract-test files because the reviewer found an authentication-mode gap and
required executable workflow regressions. No manifest, lockfile, dependency,
CLI source/test, VNC/fallback implementation, protected rollback artifact, or
out-of-scope workflow changed. The round-1 report commit changes only this file.

Round-2 remediation commit
`48bb0cc1ee52ed237183652282c9404cfdee4102` contains exactly:

- `.github/workflows/ci.yml`
- `docs/native-acceptance.md`
- `tests/surface_policy.rs`

The round-2 range changes no Rust production source, manifest, lock data,
dependency, CLI/SSH/VNC/fallback/parser behavior, fuzz scope, live-acceptance
state, or protected rollback artifact. The round-2 report commit changes only
this report.

Cleanup-proof correction commit
`0200cd8b618ed2d117f3d82c52aa9c3fd4a5e4c7` contains exactly:

- `docs/native-acceptance.md`
- `tests/surface_policy.rs`

It changes no workflow, Rust production source, manifest, lock data,
dependency, CLI/SSH/VNC/fallback/parser behavior, fuzz scope, live-acceptance
state, or protected rollback artifact. Its evidence correction commit changes
only this report.

Round-3 remediation commit
`70fef1d6aabf373c2d96f0c83b8c88129f636501` contains exactly:

- `.github/workflows/ci.yml`
- `docs/threat-model.md`
- `tests/surface_policy.rs`

The round-3 range changes no Rust production source, native acceptance recipe,
manifest, lock data, dependency, CLI/SSH/VNC/fallback/parser behavior, fuzz
scope, live-acceptance state, or protected rollback artifact. The round-3
report commit changes only this report.

Round-4 documentation correction commit
`5527325f9dfcbd21e9c5e50087f01cae2e0ad6b7` contains exactly:

- `docs/threat-model.md`

It replaces only the VNC proxy ticket resource row. It changes no runtime Rust,
test, CI, policy, configuration, acceptance behavior, dependency, manifest,
lock data, parser/fuzz scope, live state, or protected rollback artifact. The
round-4 evidence commit changes only this report.

## RED / baseline evidence

The baseline was captured at exact base
`d1d5313ef37a6b69c9161965cf32d154828e2e8c` before policy edits:

- `Cargo.lock` contained 515 packages.
- `cargo audit` failed with five vulnerabilities:
  - `RUSTSEC-2026-0194` and `RUSTSEC-2026-0195` affected both
    `quick-xml` 0.30.0 and 0.39.4 (four findings);
  - `RUSTSEC-2026-0257` affected active macOS dependency `webbrowser` 1.2.1.
- Baseline advisory output also exposed unmaintained `paste` 1.0.15 and
  `ttf-parser` 0.25.1 plus `RUSTSEC-2026-0221` on `event-listener` 5.4.1.
- `webbrowser` was target-active through
  `eframe -> egui-winit -> webbrowser`.
- `paste` came from direct `image` default AVIF/EXR features.
- `quick-xml` and `event-listener` were absent from the supported
  `aarch64-apple-darwin` graph but retained by broad all-platform metadata.
- `cargo deny list` had no repository policy and evaluated the broad lock
  graph; there was no `deny.toml`.
- The official security-policy resolver returned `[]` and no resolved root
  policy because no `SECURITY.md` existed.
- There was no merge CI workflow and the README still described a future
  minimal shell rather than the implemented client.

These are the actual RED gates used to drive the dependency, policy, CI, and
documentation changes. No parser/fuzz RED input was created because that work
is expressly parked.

## Round-1 review findings and RED/GREEN evidence

The five independently reported finding classes were:

1. Important: the advisory guard was fail-open because the graph retained tree
   prefixes, omitted `--locked` and `--all-features`, and treated graph-command
   failure like package absence.
2. Important: the SSH option set disabled password and keyboard-interactive
   authentication but did not prevent configured GSSAPI or hostbased fallback.
3. Important: `actions/checkout@v4` was mutable and persisted Git credentials.
4. Minor: the `syn@2.0.118` duplicate exception did not name both concrete
   dependency paths.
5. Minor: the `quick-xml` and `ttf-parser` threat-model claims cited only leaf
   package stanzas and omitted material lock edges.

Focused tests were added before the implementation changes. Actual RED results
at exact base `73ffcef5940488117ca7367855f183294e35076f` were:

- `every_operation_has_exact_strict_shell_free_argv` exited 101 because
  `PreferredAuthentications=publickey` was absent from the production argv;
- `ci_checkout_is_sha_pinned_without_persisted_credentials` exited 101 against
  mutable `actions/checkout@v4`; and
- `ci_advisory_guard_fails_closed_and_precedes_the_exact_audit` exited 101 with
  `cargo tree failure must fail the guard`, proving the old condition was
  fail-open.

Focused GREEN at the remediation tree was 4/4 in
`tests/ssh_command_contract.rs` and 3/3 in `tests/surface_policy.rs`. The latter
executes the extracted workflow guard against exact-argv fake graph generation
for command failure, active `quick-xml`, a clean graph, and a missing matcher,
and proves temporary-file cleanup and graph-before-audit ordering. The
controller later withdrew its proposed explicit-false documentation add-on:
the source and `tests/cli_contract.rs` intentionally retain the accurate
presence-only true flag, and neither file nor its documentation was changed.

## Dependency correction and target evidence

The minimum feature corrections were:

- `eframe` 0.31 now has `default-features = false` and only
  `default_fonts` plus `glow` directly enabled.
- Direct `image` has `default-features = false` and only `jpeg`; Tight JPEG is
  the production decoder that needs it.
- Direct `arboard` has no default features. `egui-winit` still activates its
  transitive image clipboard path; this was preserved rather than patching a
  third-party crate.
- `webbrowser` is 1.2.2 on the supported graph.

The refreshed lockfile contains 346 packages, a reduction of 169. `paste`,
`event-listener`, `quick-xml` 0.30.0, and `webbrowser` 1.2.1 are absent.
`quick-xml` 0.39.4 remains only in all-target metadata through:

```text
rustedoutclient -> eframe -> egui-winit -> smithay-clipboard
-> smithay-client-toolkit -> wayland-scanner -> quick-xml 0.39.4
```

The declared lock edges are established by `Cargo.lock:2007-2037`,
`Cargo.lock:578-610`, `Cargo.lock:628-644`, `Cargo.lock:2255-2263`,
`Cargo.lock:2228-2252`, `Cargo.lock:2849-2857`, and
`Cargo.lock:1894-1900`. Cargo.lock records the all-target package relationship;
it does not prove target activation. The actual command controls below prove
that the all-target graph contains exactly `quick-xml v0.39.4` while the
supported macOS ARM64 all-feature graph contains none.

The exact supported-target assertion produced
`supported_target_quick_xml=absent`. It immediately preceded:

```text
cargo audit --ignore RUSTSEC-2026-0194 --ignore RUSTSEC-2026-0195
```

Post-change audit scanned 346 locked dependencies and passed with zero active
vulnerabilities. Its sole allowed warning is `RUSTSEC-2026-0192`, unmaintained
`ttf-parser` 0.25.1. That package is target-active through:

```text
rustedoutclient -> egui -> epaint -> ab_glyph
-> owned_ttf_parser -> ttf-parser 0.25.1
```

Every material lock edge is recorded by `Cargo.lock:2007-2037`,
`Cargo.lock:613-625`, `Cargo.lock:673-688`, `Cargo.lock:5-13`,
`Cargo.lock:1710-1716`, and `Cargo.lock:2598-2602`.

No compatible maintained replacement exists on the accepted egui/eframe 0.31
line. Both the `quick-xml` target-inactive exception and `ttf-parser`
maintenance decision have a 2027-02-28 review deadline.

The active font-license exception path was also proved:

```text
epaint_default_fonts 0.31.1 -> epaint -> egui
```

## Cargo-deny policy and results

The graph is scoped to `aarch64-apple-darwin` with all product features.
Crates.io is the only allowed registry; unknown registries, Git dependencies,
wildcard requirements, yanked crates, and unreviewed duplicate versions are
denied. The exact Task 14 global license baseline is present.

`cargo deny list` passed. Every component and the aggregate passed:

| Command | Result |
| --- | --- |
| `cargo deny check advisories --warn unmaintained` | Pass; one visible `ttf-parser` warning |
| `cargo deny check licenses` | Pass; one visible unused MPL-2.0 baseline warning |
| `cargo deny check bans` | Pass |
| `cargo deny check sources` | Pass |
| `cargo deny check --warn unmaintained` | Pass; all four components green |

The MPL-2.0 allowance is required by the canonical Task 14 baseline even though
the current active target does not encounter that license. It is not a crate
exception. The only per-crate license exception is
`epaint_default_fonts@0.31.1` for OFL-1.1 and Ubuntu-font-1.0, with its exact
path and 2027-02-28 review date.

Duplicate-version policy denies by default and has no `skip-tree`. The six
exact target-active skips, each reviewed to 2027-02-28, are:

- `bitflags@1.3.2` (`core-graphics/winit` versus 2.13.0 in egui/glutin);
- `getrandom@0.2.17` (`rand_core/rand` versus 0.4.3 in direct runtime crates);
- `objc2@0.5.2` (eframe/winit versus 0.6.4 in arboard/glutin/webbrowser);
- `objc2-app-kit@0.2.2` (eframe/winit versus 0.3.2);
- `objc2-foundation@0.2.2` (eframe/winit versus 0.3.2); and
- `syn@2.0.118` (direct `serde -> serde_derive` versus direct
  `clap -> clap_derive -> syn@3.0.4`).

Inverse target trees were run for each skipped package, both advisory paths,
`webbrowser@1.2.2`, `ttf-parser@0.25.1`, and the font-license exception.

## CI and source policy

`.github/workflows/ci.yml` runs for pull requests and pushes to `main` with
read-only contents permission, concurrency cancellation, and a 45-minute
timeout on `macos-26`. It asserts ARM64, installs Rust 1.92.0 and exact locked
`cargo-audit` 0.22.2 / `cargo-deny` 0.19.0, then runs the formatting, locked
all-target test, warnings-denied lint, source-policy, target graph, exact audit,
deny, and locked release gates.

Checkout is pinned to
`actions/checkout@11d5960a326750d5838078e36cf38b85af677262 # v4` with
`persist-credentials: false`. The advisory guard first requires `rg`, creates a
bounded temporary graph file with exit cleanup, and explicitly fails graph
generation from this exact command:

```text
cargo tree --locked --target aarch64-apple-darwin --all-features --format '{p}' --prefix none
```

Only a successfully generated complete graph is checked for `^quick-xml v`.
The exact two-ID audit runs afterward in a separate ordered step.

The workflow has no fuzzing, artifact upload, secret, live endpoint, service,
or elevated-permission step. Local PyYAML parsing returned `ci_yaml=valid`.
The exact CI `rg` checks found no SFTP/SCP source surface and no
`StrictHostKeyChecking=no`, `StrictHostKeyChecking=accept-new`, or
`UserKnownHostsFile=/dev/null` weakening. Hosted GitHub Actions execution was
not performed and is not claimed; branch-protection enforcement remains a
repository-host setting outside this local task.

Compiled policy gates passed separately:

- `tests/fallback_contract.rs`: 15 passed, proving the single approved
  loopback-bind/listener boundary and fallback source contract;
- `tests/surface_policy.rs`: 3 passed, proving excluded product/CLI surfaces,
  SHA-pinned checkout without persisted credentials, and executable fail-closed
  advisory-guard behavior.

## Security policy, threat model, and operator docs

The official security-policy resolver after creation returned exactly:

```json
["SECURITY.md"]
```

Both repository-root and `src` scopes resolve the root `SECURITY.md`. The policy
states that no production release is currently supported, gives the public
GitHub private-reporting route, defines invariants and severity without
suppressing boundary violations, and distinguishes source controls from live
acceptance.

The standalone threat model has the required four sections, component and
effective-resource tables, a Mermaid trust-boundary diagram, explicit facts,
assumptions, open questions, residual risks, and eight attacker-story
hypotheses. Round-1 automated citation validation found 189 source citations
(108 unique path/range tuples); every referenced file and line range exists.
The sole implementation agent then reviewed the cited source ranges against
each claim. This is source-backed self-review, not independent review.

All local Markdown links resolve. The exact sensitive-keyword command:

```text
git grep -nEi 'password|ticket|private key|ssh_target|clipboard' -- docs README.md SECURITY.md
```

returned 100 lines after round-1 documentation correction. Every match was
manually classified as one of: explanatory security policy, a synthetic
`example.invalid` schema key/value, a source/type identifier, a prohibited-field
statement, or a blank acceptance field. No match contains an actual password,
ticket, private key, host clipboard value, or private configuration value.

A separate real-data scan returned zero matches for personal filesystem paths,
lab identifiers, private/RFC1918 addresses, non-synthetic email targets, SSH
key material, fingerprints, or credential assignments. The docs contain no
guest pixels, clipboard contents, stderr bodies, or environment snapshots.

## Original Task 14A implementation-head verification

All commands below ran at exact head
`6300c977eb092b455c15bf49fe97efce335bf1d6`:

| Gate | Actual result |
| --- | --- |
| `cargo fmt --all -- --check` | Pass |
| `cargo test --all-targets --all-features --locked` | 353 passed; 0 failed; 0 ignored |
| `cargo clippy --all-targets --all-features --locked -- -D warnings` | Pass |
| `cargo build --release --locked` | Pass |
| supported/all-target inverse dependency trees | Pass; paths recorded above |
| supported-target `quick-xml` assertion | Pass; absent |
| exact `cargo audit` command | Pass; 0 vulnerabilities, 1 allowed maintenance warning |
| `cargo deny list` | Pass |
| all four cargo-deny components | Pass with reviewed warnings above |
| aggregate cargo-deny | Pass |
| focused fallback/source policy | 16 passed; 0 failed |
| exact CI source `rg` checks | Pass |
| CI YAML parse | Pass locally; hosted run not claimed |
| official security-policy resolution | Exactly one root policy for root and source |
| threat citations | 173/173 file/ranges valid; claims reviewed |
| local documentation links | Pass |
| sensitive-keyword classification | 96/96 explanatory or synthetic |
| real-data documentation scan | 0 matches |
| `git diff --check` | Pass |
| implementation-head tracked status | Clean |
| exact implementation changed-file boundary | 12/12 expected files only |
| Task 14B artifacts | No tracked target/script/workflow/corpus/input; two pre-existing ignored empty directories untouched with zero files |

The 353 tests are the sum of the following test-binary results:

```text
151 + 0 + 13 + 3 + 12 + 14 + 7 + 17 + 39 + 3 + 15 + 7 + 14 + 3 +
16 + 23 + 4 + 11 + 1 = 353
```

## Round-1 remediation verification

The baseline full suite at exact fix base
`73ffcef5940488117ca7367855f183294e35076f` was 353 passed, 0 failed, and 0
ignored. After focused RED/GREEN, the complete pre-commit sequence passed with
355 tests. Every required gate was then rerun at exact committed remediation
head `5ff4be0c9db3855c06e3bc9c379dabaa392b6670`:

| Gate | Actual committed-head result |
| --- | --- |
| `cargo fmt --all -- --check` | Pass |
| `cargo test --all-targets --all-features --locked` | 355 passed; 0 failed; 0 ignored |
| `cargo clippy --all-targets --all-features --locked -- -D warnings` | Pass |
| `cargo build --release --locked` | Pass |
| `actionlint .github/workflows/ci.yml` | Pass |
| supported locked ARM64 all-feature unprefixed graph | Pass; `quick-xml` absent |
| locked all-target all-feature unprefixed positive control | Pass; exactly `quick-xml v0.39.4` |
| 10 supported inverse trees plus all-target `quick-xml` inverse | Pass |
| exact two-ID `cargo audit` | Pass; 346 dependencies, 0 vulnerabilities, 1 allowed maintenance warning |
| `cargo deny list` | Pass |
| all four cargo-deny components | Pass; reviewed `ttf-parser` and unused MPL-2.0 warnings visible |
| aggregate cargo-deny | Pass |
| exact source-policy checks | Pass |
| official security-policy resolution | Exactly `["SECURITY.md"]`; root and `src` resolve it |
| threat citations | 189/189 valid; 108 unique ranges reviewed against claims |
| local Markdown links | 24/24 valid |
| sensitive-keyword classification | 100/100 explanatory or synthetic |
| real-data documentation scan | 0 matches |
| exact remediation changed-file boundary | 10/10 expected files; CLI source/tests untouched |
| protected metadata and SHA-256 | All four exactly match the required baseline |
| Task 14B artifact boundary | 0 tracked artifacts; two pre-existing ignored directories untouched, 0 files |
| `git diff --check` and tracked status | Pass; clean |

The 355 tests are:

```text
151 + 0 + 13 + 3 + 12 + 14 + 7 + 17 + 39 + 3 + 15 + 7 + 14 + 3 +
16 + 23 + 4 + 11 + 3 = 355
```

The updated report is committed next as the required report-only commit. The
same full offline sequence is rerun after that commit; those post-report SHA and
results are necessarily supplied in the controller-facing response.

## Round-2 matcher correction and RED/GREEN evidence

Independent round 2 returned `Identity: PASS`, `Spec: FAIL`, and
`Quality: CHANGES REQUIRED`, with no Critical or Minor findings. Its one
Important finding was that the CI and native-acceptance graph checks treated
every nonzero `rg` result as package absence. Status 1 is absence, while status
2 or greater is a matcher execution, I/O, syntax, or related failure that must
not authorize the exact advisory ignores.

Tests were changed before either policy recipe at exact clean base
`1996cf7fceb4bf314625c83758e967d3d9f775e2`. Actual RED evidence was:

- the executable workflow-contract test supplied a present fake `rg` that
  returned status 2; the old workflow returned success, so the test exited 101
  with `a present matcher error must fail instead of proving package absence`;
- the initial native-checklist assertion exited 101 because the old recipe had
  no matcher preflight; and
- the strengthened native cleanup assertion exited 101 until successful
  temporary-file removal was explicitly proved before clearing the trap.

The shipped regression keeps the test total unchanged by consolidating both
policy boundaries in
`dependency_advisory_guards_fail_closed_and_precede_the_exact_audit`. It
executes the extracted CI shell with `bash -e`, exact fake Cargo argv, and
working, absent, and status-2 matchers. It proves graph-generation failure,
target-active failure, clean absence success, missing-matcher failure,
present-matcher failure, content-free output, temporary cleanup, and
graph-before-audit ordering. The non-executing native assertion checks the
parked-parser-safe documentation boundary without running the complete Step 2
recipe. Focused GREEN was 3/3 in `tests/surface_policy.rs`.

Both recipes now use the exact three-way contract:

- status 0 prints the existing target-active message and fails;
- status 1 alone proves absence and may continue; and
- every other status prints a content-free matcher-evaluation error and fails.

Both use quiet matching so neither a package line nor the full graph is
printed. CI preserves its matcher preflight, exact locked ARM64 all-feature
unprefixed graph, explicit generation failure, bounded temporary file, cleanup
trap, and graph-before-audit ordering. Native Step 2 adds the same preflight,
fails closed if temporary creation or removal fails, retains the cleanup trap
on every failure, removes the graph on success, clears the persistent trap, and
only then reaches the exact two-ID audit.

## Round-2 remediation verification

Every required gate below was rerun at exact committed implementation head
`48bb0cc1ee52ed237183652282c9404cfdee4102`:

| Gate | Actual committed-head result |
| --- | --- |
| `cargo fmt --all -- --check` | Pass |
| `cargo test --all-targets --all-features --locked` | 355 passed; 0 failed; 0 ignored |
| `cargo clippy --all-targets --all-features --locked -- -D warnings` | Pass |
| `cargo build --release --locked` | Pass |
| `actionlint .github/workflows/ci.yml` | Pass |
| supported locked ARM64 all-feature unprefixed graph | Pass; `quick-xml` absent |
| locked all-target all-feature unprefixed positive control | Pass; exact `quick-xml v0.39.4` present |
| exact two-ID `cargo audit` | Pass; 346 dependencies, 0 vulnerabilities, 1 allowed maintenance warning |
| `cargo deny check advisories --warn unmaintained` | Pass; reviewed `ttf-parser` warning visible |
| `cargo deny check licenses` | Pass; required unused MPL-2.0 baseline warning visible |
| `cargo deny check bans` | Pass |
| `cargo deny check sources` | Pass |
| `cargo deny check --warn unmaintained` | Pass; all four components green |
| focused fallback/surface/SSH contracts | 22 passed; 0 failed |
| exact source-policy checks | Pass |
| local PyYAML parse | `ci_yaml=valid` |
| official security-policy resolution | Exactly `["SECURITY.md"]`; root and `src` resolve the root policy |
| threat citations | 189/189 valid; 108 unique path/range tuples |
| local Markdown links | 24/24 valid |
| sensitive-keyword classification | 100/100 explanatory or synthetic |
| real-data documentation scan | 0 matches |
| exact round-2 implementation boundary | 3/3 allowed files only |
| protected metadata and SHA-256 | All four exactly match the required baseline |
| Task 14B artifact boundary | 0 tracked artifacts; two pre-existing ignored empty directories untouched |
| `git diff --check` and tracked status | Pass; clean |

The full total remains exactly 355 tests:

```text
151 + 0 + 13 + 3 + 12 + 14 + 7 + 17 + 39 + 3 + 15 + 7 + 14 + 3 +
16 + 23 + 4 + 11 + 3 = 355
```

The updated report is committed separately. The same complete offline gate set
is rerun at that report-only head and supplied in the controller-facing
completion response. Independent round-3 review remains pending and is not
claimed as approved. Hosted GitHub Actions, Task 14B parser smoke, live native
acceptance, fallback acceptance, rollback execution, and rollout remain
separate and unperformed.

## Controller native-cleanup proof correction

After the initial round-2 evidence commit, controller self-review identified
that the native recipe has no global `set -e`; therefore an unchecked graph
removal followed by `trap - EXIT` could authorize audit and leave residue. At
initial report head `e0937b1790fbe8b088916539158f6ef169c25ae5`, the recipe
already used an explicit failing-removal branch, but its documentation contract
asserted only token presence and ordering relative to audit. That evidence did
not structurally bind trap clearing to the successful-removal branch, so the
report's cleanup-proof claim was stronger than the shipped regression.

The focused contract was strengthened first at the exact clean initial report
head. Actual RED was exit 101 with `checked successful graph removal`, proving
that the prior recipe did not have the required success-branch shape. Commit
`0200cd8b618ed2d117f3d82c52aa9c3fd4a5e4c7` then changed only the native
recipe and its existing contract test. The recipe now performs:

```text
if graph removal succeeds
  clear the EXIT cleanup trap
else
  emit a content-free cleanup error and exit 1 with the trap still installed
```

The test proves the exact ordering: graph generation, checked removal,
success-only `trap - EXIT`, failure branch, explicit failure exit, then audit.
It does not execute the parked parser-smoke command. Focused GREEN was 3/3 in
`tests/surface_policy.rs`, and the total remains 355 because no test function
was added. The correction report is committed separately; the complete offline
gate set is rerun at that final report-only head and supplied in the
controller-facing completion response.

## Round-3 source-policy and citation correction

Independent round 3 reviewed exact clean base
`6c1be76c726128ae13db7d4e2c27d3ae31720e74` (tree
`d2e8a14e6d71e4a5801bd33853f60d95d2cd1258`) on
`feature/proxmox-console-foundation`. It returned `Identity: PASS`,
`Spec: FAIL`, and `Quality: CHANGES REQUIRED`. There were no Critical
findings. The one Important finding was that both matchers in the workflow's
`Enforce source policy` block interpreted every nonzero `rg` result as absence,
so matcher execution failure could authorize CI. The one Minor finding was that
all eight threat-model citations into that workflow still ended at the old
line numbers after the earlier dependency-policy expansion.

The executable regression was added before the workflow changed. It extracts
the actual source-policy shell block and runs six independent synthetic
scenarios: clean source, a file-transfer match, a weakened-trust match, a
file-transfer matcher status 2, a trust matcher status 2 after the first
matcher returns status 1, and a missing matcher. Both status-2 cases execute
before the test makes either assertion. Actual RED was exit 101 with:

```text
present matcher errors must fail independently: file_transfer_success=true, trust_success=true
```

That failure proves the old block authorized both matcher-error paths. The
separate citation semantic check also failed before documentation changed. Its
actual workflow-range sequence was:

```text
16-96,52-66,16-96,16-96,68-90,92-93,52-66,68-90
```

The required sequence for the final workflow layout was:

```text
16-128,52-88,16-128,16-128,90-122,124-125,52-88,90-122
```

The implementation now captures each matcher's output and status separately.
Status 0 prints the captured matching source locations plus the existing
generic policy message and fails; status 1 alone means absence and continues;
every other status discards partial matcher output, suppresses raw matcher
standard error, prints one content-free generic evaluation error, and fails.
The transfer and trust statuses remain independent, including when the first
returns 1 and the second returns 2. The dependency-graph advisory guard is
unchanged.

All eight workflow citations were then updated to the final semantic ranges.
The three broad citations cover the complete macOS job through the locked
release build, both source citations cover both explicit status branches, both
graph/audit citations cover generation plus three-way matching and the exact
audit, and the deny citation covers the exact deny step. No source claim or
control was changed merely to fit a citation.

Focused GREEN was 4/4 in `tests/surface_policy.rs`; the round-3 focused
fallback/surface/SSH set was 23/23. Every required gate below was then rerun at
exact committed implementation head
`70fef1d6aabf373c2d96f0c83b8c88129f636501` (tree
`9e7285515a55eba55a01c1a4018ce15ccb9a3557`):

| Gate | Actual committed-head result |
| --- | --- |
| `cargo fmt --all -- --check` | Pass |
| `cargo test --all-targets --all-features --locked` | 356 passed; 0 failed; 0 ignored |
| `cargo clippy --all-targets --all-features --locked -- -D warnings` | Pass |
| `cargo build --release --locked` | Pass |
| `actionlint .github/workflows/ci.yml` | Pass |
| focused fallback/surface/SSH contracts | 23 passed; 0 failed |
| exact production source-policy scans | Pass; both matchers returned status 1 |
| supported locked ARM64 all-feature unprefixed graph | Pass; `quick-xml` absent |
| locked all-target all-feature unprefixed positive control | Pass; exactly one `quick-xml v0.39.4` |
| 10 supported inverse selectors plus all-target `quick-xml` inverse | Pass |
| exact two-ID `cargo audit` | Pass; 346 dependencies, 0 vulnerabilities, 1 allowed maintenance warning |
| `cargo deny list` | Pass |
| all four cargo-deny components | Pass; reviewed `ttf-parser` and unused MPL-2.0 warnings visible |
| aggregate cargo-deny | Pass |
| local PyYAML parse | `ci_yaml=valid` |
| official security-policy resolution | Exactly `["SECURITY.md"]`; root and `src` resolve the root policy |
| threat citation ranges | 189/189 valid; 108 unique path/range tuples |
| workflow citation semantics | 8/8 valid against the final workflow controls |
| local Markdown links | 24/24 valid |
| sensitive-keyword classification | 100/100 explanatory or synthetic |
| real-data documentation scan | 0 matches |
| exact round-3 implementation boundary | 3/3 allowed files only |
| cumulative Task 14A boundary | 16/16 expected files only |
| protected metadata and SHA-256 | All four exactly match the required baseline |
| Task 14B artifact boundary | 0 tracked artifacts; two pre-existing ignored directories remain empty |
| `git diff --check` and tracked status | Pass; clean |

The 356 tests are:

```text
151 + 0 + 13 + 3 + 12 + 14 + 7 + 17 + 39 + 3 + 15 + 7 + 14 + 3 +
16 + 23 + 4 + 11 + 4 = 356
```

The one-test increase is the executable six-scenario source-policy regression.
This report is committed separately. The same complete offline sequence is
rerun at that final report-only head and supplied in the controller-facing
completion response. Independent round-4 review is pending and is not claimed
as approved.

## Round-4 proxy ticket lifetime documentation correction

Independent round 4 reviewed exact clean base
`b6294e34ac9681c7808aacf9f02f926f917d3734` (tree
`e91879dbfe9b5c3067acda710c9395f1d3122d14`) on
`feature/proxmox-console-foundation`. It returned `Identity: PASS`,
`Spec: FAIL`, and `Quality: CHANGES REQUIRED`, with no Critical or Important
findings. Its one Minor finding was that the threat model's VNC proxy ticket
resource row conflated the Rust-owned ticket lifetime with the separate copy
placed in the spawned system OpenSSH child's environment.

A documentation-specific semantic check was run before editing. The old row
failed because it did not distinguish the two owners and lacked four exact
supporting ranges. Actual RED was:

```text
ticket lifetime documentation missing: Rust-owned `ProxyTicket`, before waiting for `SecurityResult`, fallback password-file creation, `LC_PVE_TICKET` environment copy, potentially present until that exact owned proxy child exits, earlier erasure is unproved, `src/vnc/security.rs:266-279`, `src/fallback/mod.rs:345-364`, `src/ssh/command.rs:53-68`, `src/ssh/stream.rs:869-960`
```

Commit `5527325f9dfcbd21e9c5e50087f01cae2e0ad6b7` changes only that row. It now
states separately that:

- the Rust-owned `ProxyTicket` is transferred into native VNC authentication
  or consumed by explicit fallback password-file creation, and the native path
  drops its Rust owner before waiting for `SecurityResult`
  (`src/ssh/proxy.rs:25-105`, `src/vnc/security.rs:266-279`,
  `src/fallback/mod.rs:345-364`); and
- the system OpenSSH proxy child is spawned with an `LC_PVE_TICKET` environment
  copy (`src/ssh/command.rs:53-68`, `src/ssh/stream.rs:869-960`). Because no
  in-process mechanism proves earlier erasure, the child copy is conservatively
  treated as potentially present until that exact owned proxy child exits.

The row retains same-user process inspection as a residual risk, states that a
Rust drop cannot erase another process's environment, and explicitly does not
claim that the OpenSSH copy was observed to persist. This documentation-only
correction neither establishes a shorter child-environment lifetime nor changes
runtime behavior.

Focused GREEN was 15/15 required ticket-row semantics. Replacing two old row
citations with five source-specific citations changed the current threat-model
inventory from 189 citations and 108 unique ranges to 192 citations and 112
unique ranges. Historical counts above remain attached to their earlier exact
heads and were not rewritten.

Every required gate below was rerun at exact committed documentation head
`5527325f9dfcbd21e9c5e50087f01cae2e0ad6b7` (tree
`6cb0d63427421e085fe5a83f796477a8efaa0c09`):

| Gate | Actual committed-head result |
| --- | --- |
| `cargo fmt --all -- --check` | Pass |
| `cargo test --all-targets --all-features --locked` | 356 passed; 0 failed; 0 ignored |
| `cargo clippy --all-targets --all-features --locked -- -D warnings` | Pass |
| `cargo build --release --locked` | Pass |
| `actionlint .github/workflows/ci.yml` | Pass |
| focused fallback/surface/SSH contracts | 23 passed; 0 failed |
| extracted actual source-policy block | Pass |
| supported locked ARM64 all-feature unprefixed graph | Pass; `quick-xml` absent |
| locked all-target all-feature unprefixed positive control | Pass; exactly one `quick-xml v0.39.4` |
| 10 supported inverse selectors plus all-target `quick-xml` inverse | Pass |
| exact two-ID `cargo audit` | Pass; 346 dependencies, 0 vulnerabilities, 1 allowed maintenance warning |
| `cargo deny list` | Pass |
| all four cargo-deny components | Pass; reviewed `ttf-parser` and unused MPL-2.0 warnings visible |
| aggregate cargo-deny | Pass |
| local PyYAML parse | `ci_yaml=valid` |
| official security-policy resolution | Exactly `["SECURITY.md"]`; root and `src` resolve the root policy |
| threat citation ranges | 192/192 valid; 112 unique path/range tuples |
| workflow citation semantics | 8/8 valid against the unchanged workflow controls |
| ticket lifetime semantics | 15/15 valid against the corrected row and cited sources |
| local Markdown links | 24/24 valid |
| sensitive-keyword classification | 100/100 explanatory or synthetic |
| real-data documentation scan | 0 matches |
| exact round-4 documentation boundary | 1/1 allowed file only |
| cumulative Task 14A boundary | 16/16 expected files only |
| protected metadata and SHA-256 | All four exactly match the required baseline |
| Task 14B artifact boundary | 0 tracked artifacts; two pre-existing ignored directories remain empty |
| `git diff --check` and tracked status | Pass; clean |

The test total remains exactly 356:

```text
151 + 0 + 13 + 3 + 12 + 14 + 7 + 17 + 39 + 3 + 15 + 7 + 14 + 3 +
16 + 23 + 4 + 11 + 4 = 356
```

This report is committed separately. The same complete offline sequence is
rerun at that final report-only head and supplied in the controller-facing
completion response. Independent round-5 review is pending and is not claimed
as approved.

## Protected rollback artifacts and parked stash

Only metadata and SHA-256 were read for the four rollback artifacts; no file
contents were printed or inspected. Every value matches the controller's
required baseline:

| Generic path | Mode | Bytes | mtime | SHA-256 |
| --- | ---: | ---: | ---: | --- |
| `~/.local/bin/pve-vnc` | 700 | 9182 | 1786720979 | `e69819f3650f5e3632bcf7aa1d9bd991d58267ab1559d3964d6bfa947c95dd62` |
| `~/.config/pve-vnc/config.json` | 600 | 101 | 1786657951 | `f2b7d608f98d4f81fef0327abab3b1e1e8456d2042e5cceeede2ce0c903eaabc` |
| `~/Desktop/Open PVE VNC.command` | 700 | 468 | 1786658121 | `1ed2561a440ec700cd0f73946256954819035e782b8ac35077678d3f92b5e2fe` |
| `/opt/homebrew/bin/vncviewer` | 755 | 42 | 1786657823 | `ec58c3d51b44040cf9c044f3b2bee5d9cc256ef4975bb0370a66e735fb37e833` |

The controller identified the parked Task 8 stash as
`0d95b4f403abb215f8d6d9f8e21d64201d60e76a`. The Task 14A instruction forbade
even inspecting it, so no stash command was run and no independent identity
claim is made. It was neither applied, edited, dropped, recreated, nor touched.

## Self-review and remaining boundaries

- The original Task 14A source diff changes dependency features/lock data only.
  Round-1 remediation adds only four fixed public-key SSH options and their
  exact argv contracts. Round-2 remediation changes only the two graph-policy
  recipes and their executable/documentation contract test. The controller
  correction makes native trap clearing structurally success-only. Round-3
  remediation changes only CI source-policy status handling, its executable
  test, and the eight workflow citation ranges. Round-4 correction changes only
  one threat-model resource row and this evidence report; it documents distinct
  Rust-owner and child-environment lifetimes without shortening either in
  production. No public API, CLI contract, remote command, VNC, fallback,
  ticket, transport ownership, or cleanup lifecycle was changed.
- CI's only advisory ignores are the two exact target-inactive `quick-xml`
  IDs, ordered after the ARM64 graph assertion. Active `webbrowser` is upgraded
  and never ignored.
- Cargo-deny has exact source/license/duplicate controls with dated exceptions;
  no broad skip tree, Git source, wildcard, or active-target advisory ignore is
  present.
- Documentation is source-backed; all 192 source citations, 112 unique ranges,
  all eight workflow-citation semantics, and all 15 ticket-lifetime semantics
  validate. It deliberately labels hosted, live, fallback, rollback, and
  rollout evidence as unexecuted.
- No live Proxmox/VM, real clipboard, configuration contents, TigerVNC process,
  private credential, host address, fingerprint, guest pixel, or stderr body
  was accessed.
- No tracked fuzz target, workflow, script, corpus, or parser input was created
  or run. The two pre-existing ignored empty artifact directories remained
  untouched and contained zero files at each boundary check.

Remaining concerns are explicit rather than accepted silently:

1. `ttf-parser` 0.25.1 remains target-active and unmaintained until the upstream
   egui/eframe compatibility plan is resolved or reviewed by 2027-02-28.
2. `quick-xml` 0.39.4 remains in unsupported Linux/Wayland lock metadata; the
   exact audit exception is safe only while the preceding ARM64 graph assertion
   remains green, and it expires for review by 2027-02-28.
3. Hosted CI has not run at this head and branch protection was not inspected or
   changed.
4. Independent round-4 review is complete with its one Minor finding
   remediated; round-5 review is pending and not approved. Every live/native/
   fallback/rollback/rollout gate remains unexecuted.
5. Complete acceptance remains blocked on the parked Task 14B parser-smoke and
   merge-workflow tranche.
