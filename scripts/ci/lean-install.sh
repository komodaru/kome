#!/usr/bin/env bash
# Lean 4 を、固定した版の配布物から入れる。入れる先は $LEAN_HOME (既定は ~/.lean4)。
# 配布物は内容のハッシュで照合する。同じ版が入っていれば何もしない。版は lean/lean-toolchain と同じにする。
set -euo pipefail
version=4.36.0
sha256=2b7ab1ed8b92d2e6abfaf41344823b510064dbc9c0260cac73eb496ab2dea6eb
home="${LEAN_HOME:-$HOME/.lean4}"
cd "$(dirname "$0")/../.."
if [ "$(cat lean/lean-toolchain)" != "leanprover/lean4:v$version" ]; then
    echo "lean: lean/lean-toolchain の版が、入れる版 $version と違う" >&2
    exit 1
fi
if [ -x "$home/bin/lean" ] && [[ "$("$home/bin/lean" --version)" == "Lean (version $version,"* ]]; then
    echo "lean: $version は入っている"
    exit 0
fi
archive="$(scripts/ci/fetch.sh "https://github.com/leanprover/lean4/releases/download/v$version/lean-$version-linux.tar.zst" "$sha256" "$home")"
tar --zstd -xf "$archive" -C "$home" --strip-components=1
rm -f "$archive"
[[ "$("$home/bin/lean" --version)" == "Lean (version $version,"* ]]
