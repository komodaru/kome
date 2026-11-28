#!/usr/bin/env bash
# kome の全ての行を、試験のどれかが通ることを確かめる (docs/coverage.md)。
# 通らない行は、代わりに何が確かめているかを docs/uncovered.tsv に書いたものだけを認める。
set -euo pipefail
cd "$(dirname "$0")/../.."

tools="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin"
if [ ! -x "$tools/llvm-cov" ]; then
    echo "cover: llvm-tools が無い (rustup component add llvm-tools)" >&2
    exit 1
fi

out="$PWD/target/cover"
rm -rf "$out/raw"
mkdir -p "$out/raw"
export RUSTFLAGS="-C instrument-coverage"
export LLVM_PROFILE_FILE="$out/raw/%p-%m.profraw"
export CARGO_TARGET_DIR="$out/target"

# 試験の実行ファイルの一覧を取ってから、全ての試験を走らせる。
cargo test --workspace --locked --no-run --message-format=json >"$out/build.json"
cargo test --workspace --locked

"$tools/llvm-profdata" merge -sparse "$out"/raw/*.profraw -o "$out/all.profdata"
objects=()
while read -r object; do
    objects+=(--object "$object")
done < <(sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' "$out/build.json" | sort -u)
"$tools/llvm-cov" export --format=lcov --instr-profile "$out/all.profdata" "${objects[@]}" --sources "$PWD/crates/kome/src" >"$out/kome.lcov"

unset RUSTFLAGS CARGO_TARGET_DIR LLVM_PROFILE_FILE
cargo run -q --locked -p kome-lint -- cover "$out/kome.lcov"
