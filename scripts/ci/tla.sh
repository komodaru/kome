#!/usr/bin/env bash
# TLA+ のモデルを TLC で検査する。
#
# tla/expect.tsv の各行について、正しい設定は違反なしで終わること、規則を一つ外した変種は
# 名指しした不変条件の違反で終わることを確かめる。表に無い .cfg があれば失敗する。
#
# 正しい設定では、TLC にモデルの状態グラフ (全ての状態と遷移) も書き出させる。最後に、実際の
# Kernel を同じ操作で動かして得た状態と遷移が、それと過不足なく一致することを確かめる
# (crates/kome/src/conform.rs、対応の一覧は docs/tla-conformance.md)。
set -euo pipefail

cd "$(dirname "$0")/../../tla"

JAR_URL="https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar"
JAR_SHA256="936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88"
JAR="${KOME_TLA_JAR:-tla2tools.jar}"

if ! echo "$JAR_SHA256  $JAR" | sha256sum --check --status 2>/dev/null; then
    curl --fail --silent --show-error --location --retry 4 --output "$JAR" "$JAR_URL"
    echo "$JAR_SHA256  $JAR" | sha256sum --check
fi

unset JAVA_TOOL_OPTIONS
if ! diff <(grep -v '^#' expect.tsv | cut -f1 | sort) <(ls -- *.cfg | sort) >&2; then
    echo "tla/expect.tsv と tla/*.cfg が一致しない (上が差分)" >&2
    exit 1
fi

graphs="$PWD/graphs"
rm -rf "$graphs"
mkdir -p "$graphs"

failed=0
while IFS=$'\t' read -r cfg module expected; do
    case "$cfg" in '#'*|'') continue ;; esac
    dump=()
    if [ "$expected" = ok ]; then
        dump=(-dump dot "$graphs/$module.dot")
    fi
    out="$(java -XX:+UseParallelGC -cp "$JAR" tlc2.TLC -workers auto -deadlock -metadir states "${dump[@]}" -config "$cfg" "$module" 2>&1 || true)"
    if [ "$expected" = ok ]; then
        if grep -q '^Model checking completed. No error has been found.' <<<"$out"; then
            echo "ok        $cfg ($(echo "$out" | grep -o '[0-9]* distinct states found' | tail -1))"
        else
            echo "FAILED    $cfg: 違反なしで終わるはずが、そうならなかった" >&2
            echo "$out" | tail -60 >&2
            failed=1
        fi
    elif grep -q "^Error: Invariant $expected is violated\\.\$" <<<"$out"; then
        echo "ok        $cfg ($expected の反例が出た)"
    else
        echo "FAILED    $cfg: $expected の反例が出るはずが、出なかった" >&2
        echo "$out" | tail -60 >&2
        failed=1
    fi
done < expect.tsv
if [ "$failed" != 0 ]; then
    exit "$failed"
fi

# モデルの状態グラフと、実際の Kernel の動きの突き合わせ。確かめた行が出たことまで見る。
out="$(KOME_TLA_DUMPS="$graphs" cargo test --release --locked -p kome --lib conform:: -- --nocapture 2>&1)" || {
    echo "FAILED    モデルと実装の突き合わせ" >&2
    echo "$out" | grep -v '^ *[0-9]*: \|^ *at ' | tail -60 >&2
    exit 1
}
for module in $(grep -v '^#' expect.tsv | awk -F'\t' '$3 == "ok" { print $2 }' | sort -u); do
    if line="$(echo "$out" | grep -o "conform $module: .*")"; then
        echo "ok        $line"
    else
        echo "FAILED    $module: モデルと実装の突き合わせが走らなかった" >&2
        failed=1
    fi
done
exit "$failed"
