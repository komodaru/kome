#!/usr/bin/env bash
# 固定した版の配布物を取ってきて、内容のハッシュを照合し、入れる先を空にする。
# 使い方: fetch.sh <URL> <sha256> <入れる先>。取ってきたファイルの場所を出す (呼んだ側が展開して消す)。
set -euo pipefail
archive="$(mktemp)"
curl --fail --silent --show-error --location --retry 3 -o "$archive" "$1"
echo "$2  $archive" | sha256sum --check --quiet
rm -rf "$3"
mkdir -p "$3"
echo "$archive"
