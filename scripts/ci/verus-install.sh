#!/usr/bin/env bash
# Verus を、固定した版の配布物から入れる。入れる先は $VERUS_HOME (既定は ~/.verus)。
# 配布物は内容のハッシュで照合する。同じ版が入っていれば何もしない。
set -euo pipefail
version=0.2026.11.22.1ce6a59
sha256=c955d5958877f4f907cce80aa996d4239a481984829d82e325938c7bf242b0db
home="${VERUS_HOME:-$HOME/.verus}"
if grep -qs "\"$version\"" "$home/verus-x86-linux/version.json"; then
    echo "verus: $version は入っている"
    exit 0
fi
archive="$(mktemp)"
trap 'rm -f "$archive"' EXIT
curl --fail --silent --show-error --location --retry 3 -o "$archive" \
    "https://github.com/verus-lang/verus/releases/download/release/$version/verus-$version-x86-linux.zip"
echo "$sha256  $archive" | sha256sum --check --quiet
rm -rf "$home"
mkdir -p "$home"
unzip -q "$archive" -d "$home"
grep -q "\"$version\"" "$home/verus-x86-linux/version.json"
