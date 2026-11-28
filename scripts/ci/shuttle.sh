#!/usr/bin/env bash
# 参照ホスト (kome-host-std) の並列実行を、決定的シミュレーション (Shuttle) で試す。
#
# 1. 筋書きを、種から決まる進み方で数多く走らせる。全て通ること。同じ種の二度の実行で、
#    起きたことの記録が一致すること。
# 2. `--cfg loom_broken` で、状態の変化を知らせない誤りを有効にし、行き詰まりとして見つかること
#    (進み方を変えずに通っていないことの確認)。
# 3. 2 で落ちた回の種だけをもう一度たどり、同じ行き詰まりになること (落ちた回を再現できることの確認)。
set -euo pipefail
cd "$(dirname "$0")/../.."
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}/shuttle"

log="$(mktemp)"
trap 'rm -f "$log"' EXIT
RUSTFLAGS="--cfg shuttle" cargo test -p kome-host-std --test shuttle --release --locked 2>&1 | tee "$log"
scripts/ci/tests-ran.sh crates/kome-host-std/tests/shuttle.rs "$log"

broken=(cargo test -p kome-host-std --test shuttle --release --locked)
if RUSTFLAGS="--cfg shuttle --cfg loom_broken" "${broken[@]}" any_schedule >"$log" 2>&1; then
    echo "shuttle: 知らせない誤りが通った" >&2
    exit 1
fi
if ! grep -q "^deadlock! blocked tasks" "$log"; then
    echo "shuttle: 知らせない誤りが、行き詰まり以外の理由で落ちた" >&2
    cut -c1-300 "$log" | tail -40 >&2
    exit 1
fi
seed="$(grep -A2 '^failing seed:$' "$log" | tail -n 1)"
if [[ ! "$seed" =~ ^[0-9]+$ ]]; then
    echo "shuttle: 落ちた回の種が出ていない" >&2
    exit 1
fi
if KOME_SHUTTLE_SEED="$seed" RUSTFLAGS="--cfg shuttle --cfg loom_broken" "${broken[@]}" a_failing_seed_replays >"$log" 2>&1; then
    echo "shuttle: 落ちた回の種 $seed をたどり直したら通った" >&2
    exit 1
fi
if ! grep -q "^deadlock! blocked tasks" "$log"; then
    echo "shuttle: 落ちた回の種 $seed をたどり直したら、行き詰まり以外の理由で落ちた" >&2
    cut -c1-300 "$log" | tail -40 >&2
    exit 1
fi
echo "shuttle: 筋書きを通した。知らせない誤りは行き詰まりとして見つかり、種 $seed から再現できた"
