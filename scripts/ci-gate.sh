#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

run() {
  printf '\n==> %s\n' "$*"
  "$@"
}

run bash "$root/scripts/check-no-secrets.sh" "$root"

(
  cd "$root/rust"
  run cargo fmt --all -- --check
  run cargo clippy --workspace --all-targets -- -D warnings
  run env BUILD_DATE="${BUILD_DATE:-$(date +%Y-%m-%d)}" cargo build --workspace
  run cargo test --workspace
  run cargo test -p rusty-Himalaya-cli --test stream_json_contract -- --nocapture
  run cargo test -p rusty-Himalaya-cli --test output_format_contract -- --nocapture
  run python3 ./scripts/run_mock_parity_diff.py
)

(
  cd "$root/vscode-extension"
  if [[ ! -d node_modules ]]; then
    run npm ci
  fi
  run npm run test:regression -- --test-reporter=spec
  run npm run prepackage
)
