#!/usr/bin/env bash
# よく通る操作が遅くなっていないことを、二通りに判定する (docs/bench.md)。
#
# 1. 記録との比較: 基準の仕事との比が、docs/bench-gate.tsv に記録した比の 2 倍に収まること。
#    少しずつ積み重なった退行と、計算量が変わる退行を見る。機械による比の違いを吸収する幅を持つ。
# 2. 一つ前のコミットとの比較: 同じ機械で、一つ前のコミットと今のコミットを交互に測り、比が
#    1.15 倍に収まること。機械の違いが入らないので、幅を狭くできる。
#    共有の機械では、ほかの仕事に邪魔された測定が数十 % 遅くなる。邪魔は遅くする向きにしか働かない
#    ので、操作ごとに、測った中の最小値どうしを比べる。収まるまで六回まで測り足し、六回ぶんの
#    最小値でも超えているときだけ失敗にする。
#    遅くなることを受け入れた操作は、同じコミットで docs/bench-gate.tsv の行を取り直す。
#    記録の比を 1.15 倍より大きく引き上げた操作は、この比較から外す (測り直しの揺れで行が
#    少し変わっただけの操作は、外さない)。
set -euo pipefail
cd "$(dirname "$0")/../.."
root="$PWD"

cargo bench -p kome-bench --locked -- --gate docs/bench-gate.tsv

# 一つ前のコミットが無いのは、履歴を一つしか取っていないときである (CI は二つ取る)。
base="$(git rev-parse --verify 'HEAD^')"
if ! git cat-file -e "$base:scripts/ci/bench-gate.sh" 2>/dev/null; then
    echo "bench-gate: 一つ前のコミット $base はこの判定より前のもので、比を書き出せない。記録との比較だけを行った"
    exit 0
fi

work="$(mktemp -d)"
cleanup() {
    git worktree remove --force "$work/base" >/dev/null 2>&1 || echo "bench-gate: 作業用の worktree は残っていない"
    rm -rf "$work"
}
trap cleanup EXIT
git worktree add --detach "$work/base" "$base" >/dev/null

# 遅くなることを受け入れた操作: このコミットが、記録の比を 1.15 倍より大きく引き上げたもの。
git show "$base:docs/bench-gate.tsv" >"$work/before.tsv"
awk -F'\t' -v before="$work/before.tsv" '
    BEGIN { while ((getline line < before) > 0) { split(line, cell, "\t"); recorded[cell[1]] = cell[2] } }
    !/^#/ && ($1 in recorded) && $2 > recorded[$1] * 1.15 { print $1 }
' docs/bench-gate.tsv >"$work/accepted.txt"
if [ -s "$work/accepted.txt" ]; then
    echo "bench-gate: 記録の比を引き上げた操作は、一つ前のコミットと比べない:"
    sed 's/^/  /' "$work/accepted.txt"
fi

for round in 1 2 3 4 5 6; do
    (cd "$work/base" && CARGO_TARGET_DIR="$root/target/bench-base" cargo bench -p kome-bench --locked -- --record "$work/all.tsv")
    awk -F'\t' -v list="$work/accepted.txt" 'BEGIN { while ((getline name < list) > 0) accepted[name] = 1 } !($1 in accepted)' "$work/all.tsv" >"$work/base.tsv"
    if cargo bench -p kome-bench --locked -- --record "$work/head.tsv" --against "$work/base.tsv"; then
        echo "bench-gate: 一つ前のコミット $base より遅くなっていない ($round 回ぶんの最小値)"
        exit 0
    fi
done
echo "bench-gate: 六回ぶんの最小値でも、一つ前のコミット $base より遅い" >&2
exit 1
