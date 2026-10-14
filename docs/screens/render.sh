#!/bin/sh
set -eu

here=$(cd "$(dirname "$0")" && pwd)
out="$here/../screenshots"
browser=${CHROMIUM:-chromium}
mkdir -p "$out"

for page in "$here"/*.html; do
  name=$(basename "$page" .html)
  size=$(sed -n 's/.*name="size" content="\([0-9]*\)x\([0-9]*\)".*/\1,\2/p' "$page")
  "$browser" --headless --disable-gpu --hide-scrollbars \
    --force-device-scale-factor=2 --window-size="${size:-860,250}" \
    --screenshot="$out/$name.png" "file://$page" 2>/dev/null
  echo "$out/$name.png"
done
