# Lean による証明

kome は Lean に依存しない。Lean 4 のプロジェクトは `lean/` にあり (Mathlib は使わない)、
`scripts/ci/lean.sh` (CI のジョブ `lean`) が確かめる。

## 何のために使うか

| 道具 | 向いていること | kome での使い方 |
| --- | --- | --- |
| Kani | 整数の計算を、全ての値について | 番地の計算、遷移の一歩、勘定 (docs/proofs.tsv) |
| TLA+ | 少数の主体の、全ての順序 | I/O、待機、終了の競合 (tla/) |
| Lean | 操作の列の長さや主体の数に上限の無い性質 (帰納法) | 表やキューが、どんな操作の列の後でも保つ性質 (docs/lean.tsv) |

Kani と TLA+ は、上限を決めた範囲を機械が全て調べる。Lean は、上限の無い主張を証明として書く。
代わりに、Lean が扱うのは Rust のコードではなく、Lean で書いたモデルである。

## モデルと実装を結ぶもの

証明が実装について何かを言うのは、モデルが実装と同じ動きをするときだけである。それを二通りに確かめる。

- **実行して比べる**: `lean/Mirror.lean` の `mirror` は、操作の列を読んでモデルを動かし、操作のたびに
  結果の種類と、全てのプロセスの全ての頁の表への当たり方を出す。`crates/kome-testkit/tests/lean.rs` が、
  同じ操作を実際の `Kernel` と `SoftTlb` に行い、一行ずつ比べる (300 本の列、12000 個の操作。
  全ての種類の結果を通ることも確かめる)。
  最初に書いたモデルは「権限の変更は範囲の項目を必ず消す」としていた。実装は、範囲の全体がもう
  その権限なら何も変えない。この違いは、この突き合わせが見つけた。
- **上限の無い範囲は証明、モデルに無い部分は別の道具**: 番地の計算はモデルに無く、Kani が見る。
  どの定理が何を言い、何を言っていないかは `docs/lean.tsv` に書く。

## 抜け道を塞ぐもの

- 警告は失敗にする (`lean/lakefile.toml` の `warningAsError`)。証明の穴 (`sorry`) は警告なので、ビルドが通らない。
- `scripts/ci/lean.sh` は、全ての定理について、頼っている公理を Lean に出させる (`#print axioms`)。
  Lean の標準の三つ (`propext`、`Quot.sound`、`Classical.choice`) 以外に頼る定理があれば失敗する。
- lint `lean-kept` は、`lean/` に `sorry`、`axiom`、`native_decide`、`unsafe` などの語が無いこと、
  `docs/lean.tsv` の定理が実在すること、モデルを実行して比べる試験が残っていることを確かめる。

## 動かし方

```sh
scripts/ci/lean-install.sh   # 固定した版の Lean を ~/.lean4 へ入れる (LEAN_HOME で変えられる)
scripts/ci/lean.sh
```
