#!/usr/bin/env bash
# 参照ホスト (kome-host-std) の worker の進み方を Loom で試す。
#
# 二回走らせる。一回目は全て通ること。二回目は `--cfg loom_broken` で、ゲストが止まった後の
# 通知を抜いた誤りを有効にし、Loom がそれを行き詰まりとして見つけること
# (Loom が進み方を試さずに通っていないことの確認)。
set -euo pipefail
cd "$(dirname "$0")/../.."
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}/loom"

log="$(mktemp)"
trap 'rm -f "$log"' EXIT
RUSTFLAGS="--cfg loom" cargo test -p kome-host-std --test loom --release --locked 2>&1 | tee "$log"
# 書いてある検査が全て走って合格したことを照合する (絞り込みや設定の違いで黙って減らないように)。
for name in $(grep -A1 '^#\[test\]$' crates/kome-host-std/tests/loom.rs | sed -n 's/^fn \([a-z_0-9]*\)().*/\1/p'); do
    if ! grep -q "^test $name ... ok$" "$log"; then
        echo "loom: $name が走っていない" >&2
        exit 1
    fi
done

if RUSTFLAGS="--cfg loom --cfg loom_broken" cargo test -p kome-host-std --test loom --release --locked >"$log" 2>&1; then
    echo "loom: 通知を抜いた誤りが通った" >&2
    exit 1
fi
if ! grep -q "deadlock" "$log"; then
    echo "loom: 通知を抜いた誤りが、行き詰まり以外の理由で落ちた" >&2
    cut -c1-300 "$log" | tail -40 >&2
    exit 1
fi
echo "loom: 通知を抜いた誤りは行き詰まりとして見つかった"
