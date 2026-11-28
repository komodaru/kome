#!/usr/bin/env bash
# Verus で、kome の中の仕様つきの関数 (cfg_attr(verus_keep_ghost, verus_spec(..))) を確かめる。
# kome は Verus に依存しない。Verus の語彙は、Verus が自分の配布物から渡す。
#
# 二回走らせる。一回目は全て通り、証明した関数が docs/verus.tsv とちょうど同じであること。
# 二回目は `--cfg verus_broken` でわざと誤った定義を有効にし、それだけが拒まれること
# (Verus が何も確かめずに通っていないことの確認)。
set -euo pipefail
cd "$(dirname "$0")/../.."
verus="${VERUS_HOME:-$HOME/.verus}/verus-x86-linux/verus"
out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT
check=("$verus" crates/kome/src/lib.rs --crate-type=lib --crate-name kome --edition=2024 --out-dir "$out" --output-json --time-expanded)
proved() {
    grep -o '"function": "[^"]*"' "$1" | sed 's/^"function": "//; s/"$//' | sort -u
}

"${check[@]}" >"$out/clean.json"
if ! grep -q '"errors": 0,' "$out/clean.json"; then
    echo "verus: 証明できない関数がある" >&2
    exit 1
fi
listed="$(grep -v '^#' docs/verus.tsv | awk -F '\t' '$2 == "verified" || $2 == "lemma" { print $1 }' | sort -u)"
if [ -z "$listed" ] || [ "$(proved "$out/clean.json")" != "$listed" ]; then
    echo "verus: 証明した関数が docs/verus.tsv と一致しない" >&2
    proved "$out/clean.json" >&2
    exit 1
fi

broken="$(grep -rhoE 'fn verus_broken_[a-z_]+' crates/kome/src | sed 's/^fn //' | sort -u)"
if "${check[@]}" --cfg verus_broken >"$out/broken.json" 2>"$out/broken.log"; then
    echo "verus: わざと誤った定義が通った" >&2
    exit 1
fi
refused="$(proved "$out/broken.json" | grep -vxF "$listed" | sed 's/.*:://')"
if [ "$refused" != "$broken" ] || ! grep -q "\"errors\": $(echo "$broken" | wc -l)," "$out/broken.json"; then
    echo "verus: 拒まれた定義が、わざと誤った定義と一致しない" >&2
    cut -c1-300 "$out/broken.log" | tail -40 >&2
    exit 1
fi
echo "verus: $(echo "$listed" | wc -l) 個の関数と補題を証明し、わざと誤った定義 $(echo "$broken" | wc -l) 個は拒まれた"
