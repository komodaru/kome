#!/usr/bin/env bash
# Kani の証明を一つずつ動かす (並べて動かすとメモリが足りなくなる)。
#
# crates/kome/src/proofs.rs の #[kani::proof] を全て数え、どれも成功したことを確かめる。
# 一つも動かなければ失敗する。
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
        echo "ok        $harness"
    else
        echo "FAILED    $harness" >&2
        echo "$out" | tail -60 >&2
        exit 1
    fi
done
echo "${#harnesses[@]} 個の証明が成功した"
