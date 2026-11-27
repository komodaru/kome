#!/usr/bin/env bash
# Flux を、固定したコミットのソースから入れる。
# 配布されている日付つきの版は 30 日で消えるので、消えないコミットを指す。
# 入れる先は $FLUX_HOME (既定は ~/.flux)。同じコミットが入っていれば何もしない。
set -euo pipefail
commit=d1df86961db05fbba7f7c57e2f55e4399adfbc56
home="${FLUX_HOME:-$HOME/.flux}"
if ! grep -qs "sha_full = \"$commit\"" "$home/sysroot/sysroot.toml"; then
    src="$(mktemp -d)"
    trap 'rm -rf "$src"' EXIT
    git -C "$src" init -q
    git -C "$src" fetch -q --depth 1 https://github.com/flux-rs/flux "$commit"
    git -C "$src" checkout -q FETCH_HEAD
    (cd "$src" && cargo xtask install --bundle-solvers)
    grep -q "sha_full = \"$commit\"" "$home/sysroot/sysroot.toml"
fi
# sysroot をキャッシュから戻したときも、Flux が動かす版のコンパイラを用意する。
toolchain="$(sed -n 's/^toolchain = "\(.*\)"$/\1/p' "$home/sysroot/sysroot.toml")"
rustup toolchain install "$toolchain" --profile minimal --component rustc-dev --component llvm-tools --component rust-src
