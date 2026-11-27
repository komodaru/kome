#!/usr/bin/env bash
# Kani の証明を一つずつ動かす (並べて動かすとメモリが足りなくなる)。
#
# crates/kome/src/proofs.rs の #[kani::proof] を全て数え、どれも成功したことを確かめる。
# 一つも動かなければ失敗する。成功した証明には、docs/proofs.tsv の検証条件と範囲を付けて出す。
# 範囲の書かれていない証明は、成功しても合格にしない。
set -euo pipefail

cd "$(dirname "$0")/../.."

mapfile -t harnesses < <(grep -A3 '^#\[kani::proof\]' crates/kome/src/proofs.rs | sed -n 's/^fn \([a-z0-9_]*\)().*/\1/p')
if [ "${#harnesses[@]}" -eq 0 ]; then
    echo "証明が一つも見つからない" >&2
    exit 1
fi
if [ "$(grep -c '^#\[kani::proof\]' crates/kome/src/proofs.rs)" -ne "${#harnesses[@]}" ]; then
    echo "#[kani::proof] の数と、見つけた証明の数が合わない" >&2
    exit 1
fi

for harness in "${harnesses[@]}"; do
    out="$(cargo kani -p kome --harness "$harness" 2>&1 || true)"
    if echo "$out" | grep -q '^VERIFICATION:- SUCCESSFUL' && echo "$out" | grep -q '1 successfully verified harnesses, 0 failures'; then
        bounds="$(awk -F'\t' -v name="$harness" '$1 == name { print "展開の上限 " $2 "、範囲: " $3 }' docs/proofs.tsv)"
        if [ -z "$bounds" ]; then
            echo "FAILED    $harness: docs/proofs.tsv に検証条件と範囲が無い" >&2
            exit 1
        fi
        echo "ok        $harness ($bounds)"
    else
        echo "FAILED    $harness" >&2
        echo "$out" | tail -60 >&2
        exit 1
    fi
done
echo "${#harnesses[@]} 個の証明が成功した"
