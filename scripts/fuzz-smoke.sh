#!/usr/bin/env bash
set -euo pipefail

export CARGO_NET_OFFLINE=true

usage() {
  echo "usage: ./scripts/fuzz-smoke.sh <positive-seconds>" >&2
  exit 2
}

if [[ $# -ne 1 ]]; then
  usage
fi
DURATION="$1"
if [[ ! "$DURATION" =~ ^[1-9][0-9]*$ ]]; then
  usage
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TARGETS=(rfb_handshake rfb_session rfb_zrle rfb_tight rfb_hextile)
FUZZ_RUN_DIR=""
cleanup_fuzz_run_dir() {
  local dir="${FUZZ_RUN_DIR:-}"
  if [[ -z "$dir" || ! -d "$dir" ]]; then
    return 0
  fi
  case "$dir" in
    */rustedout-fuzz-smoke.??????)
      rm -rf "$dir"
      ;;
  esac
}

on_int() {
  cleanup_fuzz_run_dir
  exit 130
}

on_term() {
  cleanup_fuzz_run_dir
  exit 143
}

trap cleanup_fuzz_run_dir EXIT
trap on_int INT
trap on_term TERM



if ! command -v cargo-fuzz >/dev/null 2>&1; then
  echo "fuzz-smoke: cargo-fuzz is required" >&2
  exit 1
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "fuzz-smoke: python3 is required" >&2
  exit 1
fi
if ! command -v shasum >/dev/null 2>&1; then
  echo "fuzz-smoke: shasum is required" >&2
  exit 1
fi

for required in fuzz/Cargo.toml fuzz/Cargo.lock fuzz/rust-toolchain.toml fuzz/corpus-manifest.json; do
  if [[ ! -f "$required" ]]; then
    echo "fuzz-smoke: missing $required" >&2
    exit 1
  fi
done

FUZZ_CHANNEL="$(python3 -c 'import pathlib,re,sys
text=pathlib.Path("fuzz/rust-toolchain.toml").read_text()
match=re.search(r"channel\s*=\s*\"([^\"]+)\"", text)
sys.exit(1) if match is None else print(match.group(1))')"

if ! rustup toolchain list | grep -F "$FUZZ_CHANNEL" >/dev/null; then
  echo "fuzz-smoke: fuzz toolchain $FUZZ_CHANNEL is not installed" >&2
  exit 1
fi

shopt -s nullglob
target_files=(fuzz/fuzz_targets/*.rs)
expected_files=(
  fuzz/fuzz_targets/rfb_handshake.rs
  fuzz/fuzz_targets/rfb_session.rs
  fuzz/fuzz_targets/rfb_zrle.rs
  fuzz/fuzz_targets/rfb_tight.rs
  fuzz/fuzz_targets/rfb_hextile.rs
)
if [[ ${#target_files[@]} -ne 5 ]]; then
  echo "fuzz-smoke: fuzz/fuzz_targets must contain exactly five targets" >&2
  exit 1
fi
for expected in "${expected_files[@]}"; do
  if [[ ! -f "$expected" ]]; then
    echo "fuzz-smoke: missing $expected" >&2
    exit 1
  fi
done
for target in "${TARGETS[@]}"; do
  if [[ ! -d "fuzz/corpus/$target" ]]; then
    echo "fuzz-smoke: missing corpus directory $target" >&2
    exit 1
  fi
done
for dir in fuzz/corpus/*; do
  [[ -d "$dir" ]] || continue
  base="$(basename "$dir")"
  found=0
  for target in "${TARGETS[@]}"; do
    if [[ "$base" == "$target" ]]; then
      found=1
    fi
  done
  if [[ "$found" -ne 1 ]]; then
    echo "fuzz-smoke: unexpected corpus directory $base" >&2
    exit 1
  fi
done

python3 - "$ROOT" <<'PY'
import json, subprocess, sys
from pathlib import Path

root = Path(sys.argv[1]).resolve()
targets = ["rfb_handshake", "rfb_session", "rfb_zrle", "rfb_tight", "rfb_hextile"]
manifest = json.loads((root / "fuzz" / "corpus-manifest.json").read_text())
if manifest.get("targets") != targets:
    raise SystemExit("manifest targets drifted from the canonical five")
corpus_root = (root / "fuzz" / "corpus").resolve()
listed = set()
for seed in manifest.get("seeds", []):
    relative = Path(seed["file"])
    if relative.is_absolute() or ".." in relative.parts or relative.parts[:1] != ("corpus",):
        raise SystemExit(f"unsafe seed path {relative}")
    if len(relative.parts) != 3:
        raise SystemExit(f"seed path must be corpus/<target>/<file>: {relative}")
    _, target, name = relative.parts
    if target not in targets or name in {".", ".."} or "/" in name:
        raise SystemExit(f"invalid seed path {relative}")
    path = (root / "fuzz" / relative).resolve()
    try:
        path.relative_to(corpus_root)
    except ValueError as exc:
        raise SystemExit(f"seed path escaped corpus root: {relative}") from exc
    listed.add(path)
    if not path.is_file():
        raise SystemExit(f"missing {relative}")
    data = path.read_bytes()
    if len(data) != int(seed["length"]):
        raise SystemExit(f"length mismatch {relative}")
    digest = subprocess.check_output(["shasum", "-a", "256", str(path)], text=True).split()[0]
    if digest != seed["sha256"]:
        raise SystemExit(f"hash mismatch {relative}")
for path in corpus_root.rglob("*"):
    if path.is_file() and path.resolve() not in listed:
        raise SystemExit(f"orphan {path.relative_to(root)}")
PY


FUZZ_RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rustedout-fuzz-smoke.XXXXXX")"
case "$FUZZ_RUN_DIR" in
  */rustedout-fuzz-smoke.??????) ;;
  *)
    echo "fuzz-smoke: refused to use unexpected run directory" >&2
    exit 1
    ;;
esac
mkdir -p "$FUZZ_RUN_DIR/artifacts"

if ! cargo +"$FUZZ_CHANNEL" fmt --manifest-path fuzz/Cargo.toml -- --check >/dev/null 2>"$FUZZ_RUN_DIR/fmt.err"; then
  echo "fuzz-smoke: fuzz crate formatting check failed" >&2
  exit 1
fi

if ! cargo +"$FUZZ_CHANNEL" check --locked --offline --manifest-path fuzz/Cargo.toml --lib >/dev/null 2>"$FUZZ_RUN_DIR/offline.err"; then
  echo "fuzz-smoke: locked fuzz dependencies are not cached; run: cargo fetch --locked --manifest-path fuzz/Cargo.toml" >&2
  exit 1
fi

if ! RUSTFLAGS="--cfg fuzzing" CARGO_NET_OFFLINE=true cargo +"$FUZZ_CHANNEL" run --manifest-path fuzz/Cargo.toml --bin verify_seeds --locked --offline >"$FUZZ_RUN_DIR/verify.log" 2>&1; then
  echo "fuzz-smoke: seed verification failed" >&2
  exit 1
fi

passed=0
for target in "${TARGETS[@]}"; do
  start="$(date +%s)"
  mkdir -p "$FUZZ_RUN_DIR/corpus/$target"
  cp -R "fuzz/corpus/$target/." "$FUZZ_RUN_DIR/corpus/$target/"
  set +e
  cargo +"$FUZZ_CHANNEL" fuzz run "$target" "$FUZZ_RUN_DIR/corpus/$target" -- \
    -max_total_time="$DURATION" \
    -max_len=65536 \
    -timeout=5 \
    -rss_limit_mb=2048 \
    -artifact_prefix="$FUZZ_RUN_DIR/artifacts/" \
    >"$FUZZ_RUN_DIR/${target}.log" 2>&1
  status=$?
  set -e
  elapsed="$(( $(date +%s) - start ))"
  if [[ "$status" -eq 0 ]]; then
    echo "PASS $target (${elapsed}s)"
    passed="$((passed + 1))"
  else
    echo "FAIL $target (${elapsed}s)"
    echo "fuzz-smoke: ${passed}/5 targets passed"
    exit "$status"
  fi
done

echo "fuzz-smoke: ${passed}/5 targets passed"
