#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
versions=$HOME/.local/share/claude/versions
version=${CLAUDE_CODE_VERSION:-$(ls "$versions" | sort -V | tail -n 1)}
claude_bin=$versions/$version
image=claude-nonstop-e2e
target=$root/target/e2e

if [ ! -x "$claude_bin" ]; then
    echo "e2e: Claude Code $version not found at $claude_bin" >&2
    exit 1
fi

docker build --quiet --tag "$image" - < "$root/e2e/Dockerfile" > /dev/null
cargo fetch --locked --manifest-path "$root/Cargo.toml"
rm -rf "$target/artifacts"
mkdir -p "$target/artifacts"

echo "e2e: Claude Code $version, artifacts of failed scenarios go to $target/artifacts"

exec docker run --rm --init --network none \
    --user "$(id -u):$(id -g)" \
    --volume "$root:$root" \
    --volume "$HOME/.cargo/registry:/cargo-home/registry:ro" \
    --volume "$claude_bin:/opt/claude/$version:ro" \
    --workdir "$root" \
    --env HOME=/tmp \
    --env CARGO_TARGET_DIR="$target" \
    --env E2E_CLAUDE_BIN="/opt/claude/$version" \
    --env E2E_NONSTOP_BIN="$target/debug/claude-nonstop" \
    --env E2E_RECORDER_BIN="$target/debug/e2e-recorder" \
    --env E2E_ARTIFACTS_DIR="$target/artifacts" \
    "$image" \
    sh -c 'cargo build --locked --offline --bins -p claude-nonstop -p claude-nonstop-e2e \
        && exec cargo test --locked --offline --no-fail-fast -p claude-nonstop-e2e -- "$@"' \
    e2e "$@"
