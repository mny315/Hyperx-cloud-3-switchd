#!/usr/bin/env bash
set -Eeuo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

usage() {
  cat <<'USAGE'
Usage:
  ./run.sh                 Build release binary and run it in foreground
  ./run.sh run [ARGS...]   Build and run with daemon arguments
  ./run.sh build           Build release binary only
  ./run.sh check           Check formatting, Clippy, tests and release build
  ./run.sh integration     Test restarts/hotplug/timeouts using isolated audio
  ./run.sh fmt             Format Rust sources
  ./run.sh lock            Regenerate Cargo.lock
  ./run.sh clean           Remove Cargo build output

Examples:
  ./run.sh run --once --verbose
  ./run.sh run --verbose
  ./run.sh run --verbose --poll-ms 250 --audio-verify-secs 15
USAGE
}

need_nix() {
  if ! command -v nix >/dev/null 2>&1; then
    echo "error: nix is not available" >&2
    exit 1
  fi
}

nix_develop() {
  nix --extra-experimental-features 'nix-command flakes' \
    develop "$ROOT" --command "$@"
}

command="${1:-run}"
if (($# > 0)); then
  shift
fi

need_nix
cd "$ROOT"

case "$command" in
  run)
    exec nix --extra-experimental-features 'nix-command flakes' \
      develop "$ROOT" --command cargo run --release -- "$@"
    ;;

  build)
    nix_develop cargo build --release
    ;;

  check)
    nix_develop bash -Eeuo pipefail -c '
      cargo metadata --format-version 1 --no-deps >/dev/null
      cargo fmt --all -- --check
      cargo clippy --all-targets --all-features -- -D warnings
      cargo test --all-targets --all-features
      cargo build --release
    '
    ;;

  fmt)
    nix_develop cargo fmt --all
    ;;

  integration)
    test_runtime="$(mktemp -d /tmp/hyperx-integration-XXXXXX)"
    trap 'rm -rf -- "$test_runtime"' EXIT
    HYPERX_TEST_RUNTIME="$test_runtime" PULSE_SERVER="unix:$test_runtime/native" \
      nix_develop cargo test --locked isolated_server_recovery -- --ignored --nocapture
    ;;

  lock)
    nix_develop cargo generate-lockfile
    ;;

  clean)
    nix_develop cargo clean
    ;;

  -h|--help|help)
    usage
    ;;

  *)
    echo "error: unknown command: $command" >&2
    usage >&2
    exit 2
    ;;
esac
