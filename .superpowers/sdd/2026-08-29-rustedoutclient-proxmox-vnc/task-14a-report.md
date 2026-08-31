# Task 14A Implementation Report

## Outcome

Task 14A is complete as the non-fuzz governance and documentation tranche.
The supported macOS ARM64 dependency graph has no active vulnerability, every
configured cargo-deny component passes, the local locked Rust gates pass, and
the required security/operator documentation is present. Hosted CI, live
native acceptance, TigerVNC fallback acceptance, rollback execution, and
rollout were not performed and are not claimed.

Task 14B remains the sole deferred parser-smoke/fuzz target, script, corpus, and
workflow scope. No fuzz artifact was created or run in this task.

## Exact heads and commits

- Required clean base:
  `d1d5313ef37a6b69c9161965cf32d154828e2e8c`
- Dependency policy/CI commit:
  `6f4e935241181bc1439900a0e41656e03a638be9`
  (`chore: harden RustedOutClient dependency policy`)
- Final implementation/documentation head used for every complete gate:
  `6300c977eb092b455c15bf49fe97efce335bf1d6`
  (`docs: define RustedOutClient security and acceptance`)
- This report is committed separately as the evidence-only third commit. Its
  commit SHA is intentionally reported by the controller-facing completion
  response rather than embedded self-referentially in its own contents.

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

The third commit contains only this report. No production Rust source, test,
manifest other than `Cargo.toml`, protected rollback artifact, dependency other
than the reviewed lock refresh, or out-of-scope workflow changed.

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
quick-xml 0.39.4
└── wayland-scanner
    └── smithay-client-toolkit / wayland-client
        └── smithay-clipboard
            └── egui-winit
                └── eframe
                    └── rustedoutclient
```

The exact supported-target assertion produced
`supported_target_quick_xml=absent`. It immediately preceded:

```text
cargo audit --ignore RUSTSEC-2026-0194 --ignore RUSTSEC-2026-0195
```

Post-change audit scanned 346 locked dependencies and passed with zero active
vulnerabilities. Its sole allowed warning is `RUSTSEC-2026-0192`, unmaintained
`ttf-parser` 0.25.1. That package is target-active through:

```text
ttf-parser 0.25.1
└── owned_ttf_parser
    └── ab_glyph
        └── epaint
            └── egui
```

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
- `syn@2.0.118` (Rust derive ecosystem versus 3.0.4 in clap derive).

Inverse target trees were run for each skipped package, both advisory paths,
`webbrowser@1.2.2`, `ttf-parser@0.25.1`, and the font-license exception.

## CI and source policy

`.github/workflows/ci.yml` runs for pull requests and pushes to `main` with
read-only contents permission, concurrency cancellation, and a 45-minute
timeout on `macos-26`. It asserts ARM64, installs Rust 1.92.0 and exact locked
`cargo-audit` 0.22.2 / `cargo-deny` 0.19.0, then runs the formatting, locked
all-target test, warnings-denied lint, source-policy, target graph, exact audit,
deny, and locked release gates.

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
- `tests/surface_policy.rs`: 1 passed, proving excluded product/CLI surfaces.

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
hypotheses. Automated citation validation found 173 source citations (99 unique
path/range tuples); every referenced file and line range exists. The sole
implementation agent then reviewed the cited source ranges against each claim.
This is source-backed self-review, not independent review.

All local Markdown links resolve. The exact sensitive-keyword command:

```text
git grep -nEi 'password|ticket|private key|ssh_target|clipboard' -- docs README.md SECURITY.md
```

returned 96 lines. Every match was manually classified as one of: explanatory
security policy, a synthetic `example.invalid` schema key/value, a source/type
identifier, a prohibited-field statement, or a blank acceptance field. No
match contains an actual password, ticket, private key, host clipboard value,
or private configuration value.

A separate real-data scan returned zero matches for personal filesystem paths,
lab identifiers, private/RFC1918 addresses, non-synthetic email targets, SSH
key material, fingerprints, or credential assignments. The docs contain no
guest pixels, clipboard contents, stderr bodies, or environment snapshots.

## Complete implementation-head verification

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
| forbidden fuzz artifacts | All absent |

The 353 tests are the sum of the following test-binary results:

```text
151 + 0 + 13 + 3 + 12 + 14 + 7 + 17 + 39 + 3 + 15 + 7 + 14 + 3 +
16 + 23 + 4 + 11 + 1 = 353
```

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

- The source diff changes dependency features/lock data only; no production
  behavior, test, public API, SSH/VNC boundary, fallback implementation, or
  cleanup lifecycle was edited.
- CI's only advisory ignores are the two exact target-inactive `quick-xml`
  IDs, ordered after the ARM64 graph assertion. Active `webbrowser` is upgraded
  and never ignored.
- Cargo-deny has exact source/license/duplicate controls with dated exceptions;
  no broad skip tree, Git source, wildcard, or active-target advisory ignore is
  present.
- Documentation is source-backed and deliberately labels hosted, live,
  fallback, rollback, and rollout evidence as unexecuted.
- No live Proxmox/VM, real clipboard, configuration contents, TigerVNC process,
  private credential, host address, fingerprint, guest pixel, or stderr body
  was accessed.
- No fuzz target, workflow, script, corpus, or parser input was created or run.

Remaining concerns are explicit rather than accepted silently:

1. `ttf-parser` 0.25.1 remains target-active and unmaintained until the upstream
   egui/eframe compatibility plan is resolved or reviewed by 2027-02-28.
2. `quick-xml` 0.39.4 remains in unsupported Linux/Wayland lock metadata; the
   exact audit exception is safe only while the preceding ARM64 graph assertion
   remains green, and it expires for review by 2027-02-28.
3. Hosted CI has not run at this head and branch protection was not inspected or
   changed.
4. Independent review and every live/native/fallback/rollback/rollout gate
   remain unexecuted.
5. Complete acceptance remains blocked on the parked Task 14B parser-smoke and
   merge-workflow tranche.
