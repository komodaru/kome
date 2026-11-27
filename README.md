# kome

複数の OS Personality (Windows 互換の sake など) が共通に使うライブラリカーネル。
プロセス、スレッド、アドレス空間、資源の寿命、待機、タイマ、非同期 I/O の**状態と状態遷移**だけを持つ。

- `no_std + alloc`。依存クレートなし。`unsafe` なし。大域状態なし。
- 特定の OS・ホスト・実行基盤の語彙を持たない。ゲスト OS の意味づけ (ハンドルの番号、状態コード、
  名前空間、優先度の決め方) は Personality が、実時間・I/O の中身・メモリのバイトは Host が持つ。
- 状態は `Kernel` の値一つ。変更は `&mut Kernel` を通る操作だけで、同じ入力列からは同じ状態と同じ
  `Effect` 列になる。`Kernel` は複製できず、写し (`Snapshot`) は別の実行インスタンスとしてだけ復元できる。
- 外への働きかけは `Effect` (中断、I/O の開始・取消、資源の回収の通知) として出し、ホストの応答は
  `Completion` として受ける。操作は、将来出しうる `Effect` の席を受理の時点で予約するので、受理した
  操作の `Effect` を失わない。
- 失敗は型で分かれる: 古い ID、拒否、不足、別インスタンス、内部の障害。拒否と不足は状態を変えない。

## 構成

| 場所 | 中身 |
| --- | --- |
| `crates/kome` | カーネル本体 |
| `crates/kome-testkit` | 決定的シミュレーション (バイト列 → 操作列)、制御可能なホスト、ページごとの独立モデル |
| `crates/kome-host-std` | 参照ホスト: 実スレッドの worker が一つの錠を通して kome を動かす。Loom で進み方を試す |
| `crates/kome-bench` | 性能の測定 (`cargo bench -p kome-bench`) |
| `crates/kome-lint` | 設計上の規則の検査。規則ごとに、検出しなければならない壊し方を持つ |
| `docs/architecture.md` | 責務と依存方向、入力と出力、実行権、写しと復元 |
| `docs/memory.md` | メモリ契約: ゲストのメモリの管理と、kome 自身が確保する操作・確保しない操作 |
| `docs/invariants.toml` | 不変条件の台帳: 検査・試験・証明・モデルとの対応 |
| `docs/proofs.tsv` | Kani の証明ごとの検証条件と範囲 |
| `docs/bench.md` | 性能の測定と参考値 |
| `docs/verus.md` | Verus で証明している性質と、その入れ方 |
| `docs/flux.md` | 篩型 (Flux) で確かめている性質と、その入れ方 |
| `checks/noalloc` | 確保しないと約束した操作が確保しないことを、確保を数えて確かめる |
| `tla/` | TLA+ のモデル (I/O の寿命、待機と通知の競合、プロセスの終了処理)。遷移の表は実装から生成する |
| `fuzz/` | libFuzzer の入口 (property test と同じ `kome_testkit::drive`) |

## 確かめ方

```sh
cargo run -q -p kome-lint                                  # 設計上の規則
cargo clippy --workspace --all-targets -- -D warnings
scripts/ci/test.sh                                         # 全ての試験と、必須の試験が走ったことの照合
cargo test --manifest-path checks/noalloc/Cargo.toml       # 確保しないと約束した操作が確保しない
cargo build -p kome --target wasm32v1-none                 # OS もホストも無いターゲット
scripts/ci/tla.sh                                          # TLC。正しい設定は違反なし、誤りの変種は反例
scripts/ci/kani.sh                                         # Kani の証明を一つずつ (検証条件と範囲つき)
scripts/ci/loom.sh                                         # Loom: 参照ホストの worker の進み方
scripts/ci/verus.sh                                        # Verus: アドレスからマッピングを引く探索を、表の長さによらず
scripts/ci/flux.sh                                         # 篩型 (Flux): 範囲計算と容量の会計を全ての値について
cargo bench -p kome-bench                                  # 性能の測定 (`-- --smoke` で一度だけ)
cargo mutants -p kome --profile mutants   # 変異試験 (週次)
```

実装を変えて TLA+ の表が変わったら `KOME_UPDATE_GOLDEN=1 cargo test -p kome the_tla` で書き直す。

## 不変条件

`Kernel::audit` が、状態から全ての不変条件を数え直す。試験と決定的シミュレーションは操作のたびに
これを通す。台帳 (`docs/invariants.toml`) の各項目は、壊した状態を検査が検出する試験、守られて
いることの試験、Kani の証明、TLA+ の性質を名指しし、`kome-lint` が食い違いを検出する。

証明とモデルが示す範囲は、項目ごとに台帳に書く。Kani の証明は、整数だけの関数では全ての値に
ついて示し、表やキューを使うものは小さな上限と確保が成功した場合に限って示す (`proved` と
`docs/proofs.tsv` の「前提」)。TLA+ のモデルが調べるのは設計で、Rust のコードではない。実装から
生成した表が結ぶ範囲 (`tied`) と、それ以外を見る実装の試験 (`tested`) を分けて書く。

## 規則を足すとき

誤りを直したら、同じ誤りが戻らないようにする。型や API で防げるならそうする。防げないものは
`crates/kome-lint/src/rules.rs` に規則を足し、その規則が検出しなければならない壊し方 (`breaks`) を
一つ以上付ける。`cargo test -p kome-lint` が、壊し方ごとに検出を確かめる。
