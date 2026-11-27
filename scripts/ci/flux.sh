#!/usr/bin/env bash
# Flux (篩型) で、Cargo.toml の [package.metadata.flux] が挙げる定義を確かめる。
# kome は flux-rs に依存しない。標準ライブラリの篩型 (flux_core) は Flux の sysroot から読む。
#
# 二回走らせる。一回目は全て通ること。二回目は `--cfg flux_broken` でわざと誤った定義を
# 有効にし、その定義だけが全て拒まれること (Flux が何も確かめずに通っていないことの確認)。
set -euo pipefail
cd "$(dirname "$0")/../.."
sysroot="${FLUX_SYSROOT:-$HOME/.flux/sysroot}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}/flux"
# cargo-flux は sysroot の中にある。CI がキャッシュから戻すのは sysroot だけなので、そこから探す。
export PATH="$sysroot/bin:$PATH"

RUSTFLAGS="-L $sysroot/lib" cargo flux -p kome

log="$(mktemp)"
trap 'rm -f "$log"' EXIT
if RUSTFLAGS="-L $sysroot/lib --cfg flux_broken" cargo flux -p kome >"$log" 2>&1; then
    echo "flux: わざと誤った定義が通った" >&2
    exit 1
fi
refused="$(grep -o "only-check='def:[^']*'" "$log" | sort -u | sed "s/only-check='def://; s/'$//")"
expected="$(grep -rhoE 'fn flux_broken_[a-z_]+' crates/kome/src | sed 's/^fn //' | sort -u)"
if [ "$(echo "$refused" | sed 's/.*:://' | sort -u)" != "$expected" ]; then
    echo "flux: 拒まれた定義が、わざと誤った定義と一致しない" >&2
    echo "拒まれた: $refused" >&2
    echo "期待: $expected" >&2
    cut -c1-300 "$log" | tail -40 >&2
    exit 1
fi
echo "flux: わざと誤った定義 $(echo "$expected" | wc -l) 個は全て拒まれた"
