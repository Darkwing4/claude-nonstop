#!/bin/sh
set -eu

repo="Darkwing4/claude-nonstop"
dir="${CLAUDE_NONSTOP_INSTALL_DIR:-$HOME/.local/bin}"
version="${CLAUDE_NONSTOP_VERSION:-latest}"

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64|Linux-arm64) target=aarch64-unknown-linux-musl ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  *) echo "unsupported platform: $os $arch" >&2; exit 1 ;;
esac

if [ "$version" = latest ]; then
  version=$(curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest" | sed 's#.*/##')
fi
base="https://github.com/$repo/releases/download/$version"
name="claude-nonstop-$version-$target"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fsSL "$base/$name.tar.gz" -o "$tmp/$name.tar.gz"
curl -fsSL "$base/$name.tar.gz.sha256" -o "$tmp/$name.tar.gz.sha256"
cd "$tmp"
if command -v sha256sum >/dev/null 2>&1; then sha256sum -c "$name.tar.gz.sha256"; else shasum -a 256 -c "$name.tar.gz.sha256"; fi
tar xzf "$name.tar.gz"
mkdir -p "$dir"
install -m 0755 "$name/claude-nonstop" "$dir/claude-nonstop"

echo "installed $dir/claude-nonstop ($version)"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "add $dir to PATH first" ;;
esac
echo "next: claude-nonstop install"
