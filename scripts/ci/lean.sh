#!/usr/bin/env bash
# Lean 4 で、lean/ の定理を確かめる。kome は Lean に依存しない (docs/lean.md)。
#
# 1. 全ての定理をビルドする (証明の検査)。警告は失敗にする (lean/lakefile.toml)。
# 2. 全ての定理について、頼っている公理を Lean に出させる。Lean の標準の三つ (propext、
#    Quot.sound、Classical.choice) 以外に頼る定理があれば失敗する。証明の穴 (sorry) は、
#    ここで sorryAx という公理として現れる。
# 3. モデルを実行する `mirror` の出力を、実際の Kernel と SoftTlb の動きと比べる
#    (crates/kome-testkit/tests/lean.rs)。照合した行が出なければ失敗する。
set -euo pipefail
cd "$(dirname "$0")/../.."
home="${LEAN_HOME:-$HOME/.lean4}"
export PATH="$home/bin:$PATH"

(cd lean && lake build)

audit="$(mktemp --suffix=.lean)"
trap 'rm -f "$audit"' EXIT
echo "import Kome" >"$audit"
count=0
for file in lean/Kome/*.lean; do
    module="$(basename "$file" .lean)"
    while read -r name; do
        echo "#print axioms Kome.$module.$name" >>"$audit"
        count=$((count + 1))
    done < <(sed -n 's/^theorem \([A-Za-z0-9_.]*\).*/\1/p' "$file")
done
if [ "$count" -eq 0 ]; then
    echo "lean: 定理が一つも見つからない" >&2
    exit 1
fi
out="$(cd lean && lake env lean "$audit")"
if [ "$(grep -c "^'Kome\." <<<"$out")" -ne "$count" ]; then
    echo "lean: 公理を調べた定理の数が、見つけた定理の数 $count と合わない" >&2
    echo "$out" >&2
    exit 1
fi
other="$(tr '\n' ' ' <<<"$out" | sed "s/'Kome\./\n'Kome./g" | sed 's/propext//g; s/Quot\.sound//g; s/Classical\.choice//g' | grep 'depends on axioms: \[[^]]*[A-Za-z]' || echo "")"
if [ -n "$other" ]; then
    echo "lean: 標準の公理以外に頼る定理がある (証明の穴は sorryAx として出る)" >&2
    echo "$other" >&2
    exit 1
fi
echo "lean: $count 個の定理を確かめた。標準の公理以外には頼っていない"

out="$(KOME_LEAN_MIRROR="$PWD/lean/.lake/build/bin/mirror" cargo test --locked -p kome-testkit --test lean -- --nocapture 2>&1)" || {
    echo "$out" >&2
    exit 1
}
if ! grep -q '^lean: .*Lean のモデルと実装の動きを照合した$' <<<"$out"; then
    echo "lean: モデルと実装を照合した行が出ていない" >&2
    echo "$out" >&2
    exit 1
fi
grep '^lean: ' <<<"$out"
