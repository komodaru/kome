#!/usr/bin/env bash
# 試験のファイルに書いてある検査が、記録の中で全て走って合格したことを照合する
# (絞り込みや設定の違いで、黙って減らないように)。
#
# 使い方: tests-ran.sh <試験のファイル> <cargo test の記録>
set -euo pipefail
tests="$1"
log="$2"
count=0
for name in $(grep -A1 '^#\[test\]$' "$tests" | sed -n 's/^fn \([a-z_0-9]*\)().*/\1/p'); do
    if ! grep -q "^test $name ... ok$" "$log"; then
        echo "$tests: $name が走っていない" >&2
        exit 1
    fi
    count=$((count + 1))
done
if [ "$count" -eq 0 ]; then
    echo "$tests: 検査が一つも見つからない" >&2
    exit 1
fi
