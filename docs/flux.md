# 篩型 (Flux)

kome は、値の範囲で決まる性質を [Flux](https://github.com/flux-rs/flux) の篩型で書き、
コンパイル時に全ての入力について確かめる。試験や Kani の証明が「ある大きさまで」を見るのに対し、
篩型は関数の型として性質を持ち回るので、呼び出す側の検査にもそのまま使える。

## 確かめていること

| 場所 | 型が言っていること |
| --- | --- |
| `GuestRange` (`addr.rs`) | 先頭は末尾以下。`new` が範囲を返すのは、長さが 0 でなく末尾が `u64` に収まるときちょうどで、そのとき末尾は `start + len - 1`。`spanning` は `first <= last` のときちょうど。`byte_len` は `last - start + 1` で、返せないのは範囲が `u64` 全体のときだけ。`contains` と `overlaps` は両端を含む比較そのもの |
| `Seats` (`effect.rs`) | 予約の数は上限以下。`room_for` と `promise` が成功するのは `queued + promised + count <= limit` のときちょうどで、成功すれば予約は `count` 増え、失敗すれば変わらない。`forgo` は予約より多くは返せない。`within_limit` は `queued + promised <= limit` そのもの |
| `Arena` (`id.rs`) | 生きている資源の数は上限以下。`reserve` が成功すれば一つ、`reserve_many` が成功すれば `count` 個ぶんの空きがある。`insert` は成功したときだけ数を一つ増やし、`remove` は数を増やさず、取り除けたら一つ減らす |

あふれの検査は `strict` にしてある。確かめる定義の中の足し算と引き算は、あふれないことも証明される。

ID の世代と発行元の照合 (古い ID が何も引かないこと) は、世代の型 (`NonZeroU32`) に Flux の篩型が
無いので、ここでは書いていない。そちらは Kani の証明と決定的シミュレーションが見る
(`docs/proofs.tsv`)。

## 依存を増やさない

属性は `#[cfg_attr(flux, flux::spec(..))]` の形で書く。普通のコンパイルでは消えるので、kome は
`flux-rs` に依存しない。`checked_add` などの標準ライブラリの篩型は、Flux で確かめるときだけ
`extern crate flux_core` で読み、その場所は `scripts/ci/flux.sh` が Flux の sysroot から渡す。

確かめる範囲は `crates/kome/Cargo.toml` の `[package.metadata.flux]` の `include` が決める。
仕様の数は `docs/flux.tsv` が持ち、kome-lint の `refinements-kept` が、仕様が減っていないこと、
仕様を書いたファイルが `include` に入っていること、無視の指定が無いことを照合する。

## 確かめずに信じる定義

`flux::trusted` は一つだけある。

| 定義 | 信じていること | 理由 |
| --- | --- | --- |
| `effect::wide` | `u32` を `u64` に広げても値が変わらない | Flux は `u64::from` の結果を知らない。本体は `u64::from(value)` の一行 |

## Flux が黙って通っていないことの確認

`scripts/ci/flux.sh` は二回走らせる。二回目は `--cfg flux_broken` で、確かめるファイルごとに
一つ置いた、わざと誤った定義 (`fn flux_broken_..`) を有効にし、それらがちょうど全て拒まれることを見る。
`include` の書き方を誤って Flux が何も確かめなくなれば、ここで落ちる。

## 入れ方

```
scripts/ci/flux-install.sh   # 固定したコミットをソースから ~/.flux へ入れる
scripts/ci/flux.sh
```

配布されている日付つきの版は 30 日で消えるので、CI は消えないコミットを指してソースから作り、
`~/.flux` をキャッシュする。
