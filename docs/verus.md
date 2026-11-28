# Verus

長さに上限の無い表についての性質は、[Verus](https://github.com/verus-lang/verus) で証明する。
Kani の証明は表の大きさに小さな上限を置き、Flux の篩型は値の範囲までしか言えない。Verus は
繰り返しの不変条件と量化子を書けるので、「どんな長さの表でも」を示せる。

## 証明していること

アドレス空間のマッピングの表は、先頭アドレスの昇順で、重ならない (台帳 MEM-2。`Kernel::audit` が
検査する)。その前提のもとで、アドレスからマッピングを引く二分探索 (`crates/kome/src/lookup.rs`) に
ついて、表の長さによらず次を示す。

| 関数 | 示していること |
| --- | --- |
| `first_ending_at_or_after` | 返した位置より前のマッピングは全て `addr` より前に終わり、返した位置以降は全て `addr` 以降に終わる。繰り返しは必ず止まり、位置の計算はあふれない |
| `containing` | `Some(at)` なら `at` 番目のマッピングが `addr` を含む。`None` なら `addr` を含むマッピングは表のどこにも無い |

| `free_at` | 真なら、`at` 番目から後ろのマッピングは全て `last` より後に始まる。偽なら、`at` 番目が `last` 以前に始まる。`at` が `first_ending_at_or_after` の返した位置なら、真は「その位置へ入れられる」、偽は「`at` 番目と重なる」である |

証明するのは実行されるコードそのもので、写しではない。`Space` がアドレスを引く経路
(`translate` など) と、`map` が入れる位置を決める所は、この三つの関数だけを通る。

## 表の更新が、昇順で重ならないことを保つ

`map`、`unmap`、`protect` が表を書き換える形は、次の五つに限られる。それぞれについて、書き換える
前の表が昇順で重ならなければ、書き換えた後の表もそうであることを、表の長さによらず示す
(実行しない証明だけの定義 `proof fn`。一覧では `lemma`)。

| 補題 | 書き換えの形 | 使う所 |
| --- | --- | --- |
| `inserting_into_a_gap_keeps_the_order` | `free_at` が真を返した位置へ項目を入れる | `map` |
| `removing_keeps_the_order` | 項目を一つ取り除く | `unmap` (範囲に収まる項目) |
| `narrowing_keeps_the_order` | 項目を、元の範囲の内側へ縮める | `unmap` と `protect` (範囲の端をまたぐ項目) |
| `splitting_keeps_the_order` | 項目を、間の空いた二つへ分ける | `unmap` と `protect` (範囲が一つの項目の内側) |
| `joining_keeps_the_order` | 隣り合う二つを、前の先頭から後ろの末尾までの一つにまとめる | `protect` (続いている項目) |

補題が言うのは、列 (`Seq`) の上の書き換えについてである。`mem.rs` の書き換えがこの五つの形で
あること、`List` の `insert`、`remove` が列の同じ操作であることは、Verus では確かめていない。
そこは `Kernel::audit` (操作のたびに表を数え直す)、決定的シミュレーション、Kani の証明が見る。

## 依存を増やさない

仕様は `#[cfg_attr(verus_keep_ghost, verus_spec(..))]` の形で書く。普通のコンパイルでは消えるので、
kome は Verus に依存しない。Verus の語彙 (`vstd` など) は、Verus で確かめるときだけ
`extern crate` で読み、Verus が自分の配布物から渡す。

## 確かめずに信じていること

- `Mapping` は、Verus からは中身の見えない型として扱う。先頭と末尾は `Mapping::start` と
  `Mapping::last` で読み、この二つは本体を確かめずに仕様だけを置く (欄をそのまま返す一行)。
- 表が昇順で重ならないこと (前提) を実装が保つことは、`Kernel::audit`、決定的シミュレーション、
  Kani の証明が見る。Verus が示すのは「前提が成り立てば、引いた結果は正しい」と「五つの形の
  書き換えは前提を保つ」である。
- `<[T]>::get`、`saturating_add` などの標準ライブラリの仕様は、Verus の `vstd` のものを使う。

扱う関数の一覧は `docs/verus.tsv` にあり、kome-lint の `verus-kept` が、仕様が減っていないこと、
信じる定義が増えていないこと、証明を飛ばす指定 (`assume` など) が無いことを照合する。

## Verus が黙って通っていないことの確認

`scripts/ci/verus.sh` は二回走らせる。一回目は、証明した関数が `docs/verus.tsv` とちょうど同じで
あることを見る。二回目は `--cfg verus_broken` で、わざと誤った定義 (`fn verus_broken_..`) を有効にし、
それだけが拒まれることを見る。

## 入れ方

```
scripts/ci/verus-install.sh   # 固定した版の配布物を、ハッシュを照合して ~/.verus へ入れる
scripts/ci/verus.sh
```

Verus が使うコンパイラは 1.100.0 で、kome の `rust-toolchain.toml` と同じである。
