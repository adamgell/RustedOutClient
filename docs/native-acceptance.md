# Native Acceptance and Reversible Rollout

**Status: NOT READY and NOT EXECUTED.** Local synthetic verification is not
live acceptance. Task 14B parser-smoke files exist at this branch (S1:
implementation present). The parser gate is not marked passed. Hosted smoke,
independent review, and live native/fallback acceptance remain separate.
This file is the sanitized operator checklist for a later separately
authorized run.

Keep local/CI readiness, independent review, native lab acceptance, TigerVNC
fallback acceptance, rollback proof, and rollout as separate results. Never
record credentials, addresses, fingerprints, guest pixels, clipboard text,
raw stderr, or environment snapshots.

## Exact gate sequence

### Step 1: Verify exact local and remote heads before acceptance

At the approved feature branch, record a clean worktree, push/fetch only when
authorized, record local and remote heads, and run `git diff --check`.

Expected: the tracked worktree is clean, local and remote approved heads match
exactly, and the diff check emits nothing.

### Step 2: Run the complete verification suite at that head

Run and record:

```bash
cargo fmt --all -- --check
cargo test --all-targets --all-features --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
if ! command -v rg >/dev/null 2>&1; then
  echo "Required dependency-policy matcher is unavailable"
  exit 1
fi
graph_file="$(mktemp "${TMPDIR:-/tmp}/rustedoutclient-supported-graph.XXXXXX")" || {
  echo "Could not create the supported dependency graph file"
  exit 1
}
trap 'rm -f "$graph_file"' EXIT
if ! cargo tree --locked --target aarch64-apple-darwin --all-features --format '{p}' --prefix none > "$graph_file"; then
  echo "Could not generate the supported macOS ARM64 dependency graph"
  exit 1
fi
matcher_status=0
rg -q '^quick-xml v' "$graph_file" || matcher_status=$?
case "$matcher_status" in
  0)
    echo "quick-xml unexpectedly entered the supported macOS ARM64 graph"
    exit 1
    ;;
  1)
    ;;
  *)
    echo "Could not evaluate the supported macOS ARM64 dependency policy"
    exit 1
    ;;
esac
if rm -f "$graph_file"; then
  trap - EXIT
else
  echo "Could not remove the supported dependency graph file"
  exit 1
fi
cargo audit --ignore RUSTSEC-2026-0194 --ignore RUSTSEC-2026-0195
cargo deny check --warn unmaintained
./scripts/fuzz-smoke.sh 30
cargo build --release --locked
shasum -a 256 target/release/rustedoutclient
```

Expected: all gates pass and the record contains the exact HEAD and release
binary SHA-256. The graph command must prove `quick-xml` absent from the
supported target before the exact audit exception is used.

**Parser-smoke state:** scripts, five fuzz targets, corpus, workflow, and
contract tests exist. `./scripts/fuzz-smoke.sh 30` is the operator interface.
Approved pins: Rust 1.92.0, `nightly-2026-08-29-aarch64-apple-darwin`,
`cargo-fuzz` 0.13.2. The workflow is a candidate required check pending
separate operator authorization. Do not mark this step passed without
exact-head local and hosted evidence.

### Step 3: Run an independent code review

Review the exact feature-branch diff independently. Resolve every actionable
correctness or security finding with a focused test and commit, rerun Step 2,
and update the exact-head values. Treat CI, review, dependency audit, parser
smoke, and live native acceptance as separate gates.

### Step 4: Establish the side-by-side local install

```bash
install -m 0755 target/release/rustedoutclient ~/.local/bin/rustedoutclient
~/.local/bin/rustedoutclient --version
```

Expected: the new binary reports its version and the old
`~/.local/bin/pve-vnc` metadata/hash remain unchanged.

### Step 5: Migrate non-secret configuration explicitly

Launch RustedOutClient, inspect any proposed import of only `ssh_target`,
`node`, and fallback viewer, and save only after confirming no password or
ticket field is displayed or written. Verify mode `0700` on the app directory
and `0600` on `config.json`. Do not print the file.

### Step 6: Establish the existing TigerVNC baseline

With a separately approved synthetic placeholder standing for the real
selector, run the protected old route:

```bash
~/.local/bin/pve-vnc list
~/.local/bin/pve-vnc open <approved-selector>
```

Observe inventory, viewer connection, framebuffer, pointer, and normal
keyboard behavior. Record operator-Open to visible-guest timing by stopwatch,
retaining only the content-free elapsed values.

### Step 7: Perform native RustedOutClient acceptance

```bash
~/.local/bin/rustedoutclient list
~/.local/bin/rustedoutclient probe <approved-selector> --json
~/.local/bin/rustedoutclient open <approved-selector>
```

Visibly verify exact VM selection; boot, lock, and desktop framebuffer
correctness; pointer accuracy; normal keyboard input; the exact six-event
Ctrl-Alt-Delete sequence; Release All Keys recovery; fullscreen; Fit; 1:1;
view-only; clipboard default-off; bounded explicit Send and Receive when
enabled; proxy-kill reconnect with a distinct ticket; preload/cache behavior;
and no duplicate tab for the same VM. Verify failures for unknown/changed SSH
host trust and for unsupported RFB security modes expose typed, content-free
errors and do not downgrade.

Using read-only evidence, record the VM's existing virtual display device and
guest video-driver family. Do not alter VM hardware or install a driver. Enable
Dynamic Resolution and verify:

- a `1600x900` backing-pixel viewport requests `1600x896`;
- a `1920x1080` viewport requests and, when supported, applies that size;
- the current usable fullscreen backing-pixel viewport is normalized and
  tested; and
- each request visibly transitions through `Requested`, `Pending`, and either
  `Applied`, `Rejected`, `Unsupported`, or `Timed out` within the contract.

For `Rejected`, `Unsupported`, and timeout outcomes, verify the session remains
connected and Fit still works. If Applied requires VM hardware or guest-driver
changes, stop and request separate authorization.

### Step 8: Measure warm-open and concurrency acceptance

Run `probe <approved-selector> --json` ten times after the SSH master is warm
and record only `first_frame_ms`. Confirm the median is at most 2,000 ms and at
least 30 percent below the ten observed Python/TigerVNC baseline timings. Open
a second distinct running VM and verify both sessions remain responsive and
inputs stay session-local.

### Step 9: Prove repeated cleanup and fallback

Open and close the native session ten times. After each close, use app
diagnostics plus read-only process/listener inspection to prove no owned proxy
child, VNC task, listener, password file, clipboard slot, or completed-session
runtime artifact remains. Do not kill unrelated processes.

Then choose the explicit **Open in TigerVNC** action. Verify the one-loopback-
peer SSH relay works and that the listener, viewer, proxy, executable snapshot,
password file, and runtime artifacts are all cleaned. Record fallback as its
own result; it does not satisfy the native result.

### Step 10: Re-prove rollback and record the acceptance boundary

Close RustedOutClient and run:

```bash
~/.local/bin/pve-vnc open <approved-selector>
```

Expected: the old workflow still works and protected metadata/hashes are
unchanged. Record exact commit, binary SHA-256, timings, sanitized VM labels,
macOS version/architecture, TigerVNC version, and observed UI transitions in a
new sanitized acceptance record. Exclude all private or content-bearing data.

### Step 11: Commit acceptance evidence and prove the final exact head

Commit only the updated checklist and sanitized acceptance record, push only
when authorized, rerun Step 1, and wait for every required hosted CI check.
Do not replace the old helper or Desktop launcher in this sequence.

## Blank sanitized evidence record

Create a dated record from this table only when the run is authorized. Leave a
field blank rather than inserting a real address, fingerprint, credential,
guest image, clipboard value, stderr body, or environment value.

| Gate | Sanitized evidence field | Result |
| --- | --- | --- |
| Exact head | Local SHA / remote SHA / clean diff |  |
| Local suite | Formatting / tests and count / lint / release SHA-256 |  |
| Dependency policy | Supported-target graph / audit / deny |  |
| Parser smoke | Duration / workflow run | Implementation present; not marked passed without exact-head evidence |
| Independent review | Reviewer / reviewed SHA / findings closed |  |
| Side-by-side install | Version / old helper metadata unchanged |  |
| Configuration | Directory mode / file mode / no secret fields |  |
| Baseline | Ten content-free timing values / median |  |
| Native selection/framebuffer | Sanitized VM label / visible transitions |  |
| Input | Pointer / keyboard / CAD / Release All / view-only |  |
| Clipboard | Default off / explicit Send / explicit Receive / bound |  |
| Dynamic Resolution | 1600x896 / 1920x1080 / fullscreen / outcome states |  |
| Failure handling | Trust failure / unsupported security / reconnect |  |
| Warm and concurrent | Ten first-frame values / median / second session |  |
| Cleanup | Ten native cycles / owned tasks-processes-files-listeners |  |
| TigerVNC fallback | Manual launch / loopback / complete cleanup |  |
| Rollback | Old route result / all protected metadata unchanged |  |
| Hosted CI and rollout | Final remote SHA / required checks / operator decision |  |

No build, synthetic peer, one live connection, fallback result, or rollback
result alone completes acceptance. Completion requires all eleven steps at one
reviewed exact head, including exact-head parser-smoke evidence.
