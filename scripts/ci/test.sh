#!/usr/bin/env bash
# 全ての試験を動かし、必須の試験が実際に走って合格したことを出力と照合する。
#
# 必須の試験は、全ての #[test]、台帳 (docs/invariants.toml) が名指しする試験、
# docs/compile-fail.tsv が数える compile_fail 試験。無視や絞り込みで走らなかった試験は
# 合格に数えない (crates/kome-lint/src/ran.rs)。
set -euo pipefail

cd "$(dirname "$0")/../.."

mkdir -p target
log="target/test-output.txt"
cargo test --workspace --locked 2>&1 | tee "$log"
cargo run -q --locked -p kome-lint -- ran "$log"
