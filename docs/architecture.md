# 責務と依存方向

kome は、複数の OS Personality に共通する状態管理だけを持つライブラリカーネルである。
この文書は、kome が何を所有し、何を所有しないか、外とどうつながるかを定める。
ここに書いた規則のうち機械で確かめられるものは `kome-lint` と CI が確かめる (各節の「検査」)。

## 依存方向

```
ゲーム用製品 → sake (実行統括・Personality・サービス・ホスト実装) → kome
```

- kome は sake を参照しない。依存クレート・機能フラグ・ビルドスクリプトを持たない。
  検査: lint `no-deps`。
- kome の通常のビルドは `no_std + alloc`。時計・ファイル・ホストのスレッドを読まない。
  検査: lint `no-std`、CI の `wasm32v1-none` と `thumbv7em-none-eabi` のビルド。
- kome は特定の OS・ホスト・実行基盤の語彙 (関数名、構造体、状態コード) を持たない。
  検査: lint `host-vocabulary`。
- 試験基盤 (`kome-testkit`) と規則の検査 (`kome-lint`) は `std` を使う。kome はどちらにも依存しない。

## kome が所有するもの

機構ごとに、所有するもの、満たす契約 (台帳 `docs/invariants.toml` の識別子)、置き場所を並べる。
右端の列は、同じ機構のうち kome が持たず、Personality・サービス・ホスト実装が持つものである。

| 機構 | kome が所有するもの | 契約 | 場所 | kome の外が所有するもの |
| --- | --- | --- | --- | --- |
| プロセス | 所属するスレッドと資源、アドレス空間、寿命、終了処理と回収 | PROC-1 | `process.rs` | 実行形式、モジュール、起動環境 |
| スレッド | 実行・待機・サービス中の状態、停止要求、終了要求、実行権 | RUN-1、RUN-2 | `thread.rs`、`sched.rs` | スレッド環境の ABI、例外や通知の規則 |
| メモリ | マッピング、実体への参照、範囲、権限、世代 | MEM-1、MEM-2、MEM-3 | `mem.rs`、`addr.rs` | 確保 API の意味、内容のバイト列 |
| 資源 | 型つきの世代つき ID、参照と固定、破棄できる時点 | ID-1、OBJ-1 | `id.rs`、`object.rs` | ゲストに見せる番号と、その再利用の規則 |
| 待機 | 登録、条件の評価、結果の確定、期限との競合 | WAIT-1、WAIT-2 | `wait.rs` | 個々の同期オブジェクトの意味 |
| 時刻 | 仮想の単調時刻、期限の列、タイマ | TIME-1 | `time.rs`、`timer.rs` | 日時、時差、実時間との対応 |
| I/O | 要求の ID、進行状態、取消要求、終端の確定、公開 | IO-1 | `io.rs` | 要求の中身、出力の書式、完了の通知方式 |
| 上限 | 表・キュー・要求数・実行量の上限 | FX-1 | `limits.rs`、`effect.rs` | 値を決める方針 |
| 失敗の区別 | 古い ID・拒否・不足・別インスタンス・内部の障害 | ERR-1 | `error.rs` | ゲストへ返す値への変換 |

プロセス間の通知・メッセージ・共有領域のうち、共有領域は実体 (backing) を複数のプロセスが
保持して対応づけることで表す。通知は待機可能な資源で表す。メッセージの列は、それを必要とする
契約が決まったときに、この表へ行を足してから実装する。

## 入力と出力

- 状態は `Kernel` の値一つ。変更は `&mut Kernel` をとる操作だけが行う。操作は全て `Input` の
  変種としても表せる (`Kernel::apply`)。検査: lint `operations-covered`。
- `Effect` の取り出し (`poll_effect`、`Input::PollEffect`) も入力である。取り出すと `Effect` の席が
  空き、後の操作が受理されるかどうかが変わるので、記録・再生は取り出した位置も入力の列に持つ。
- 状態の更新に入る情報は、操作の引数、ホストからの `Completion`、`Kernel::advance` に渡す
  仮想時刻だけである。大域状態と内部可変性を持たない。検査: lint `no-globals`。
- 外への働きかけは `Effect` として出す。受理した操作が出しうる `Effect` の席は受理の前に
  予約し、足りなければ受理の前に `Exhausted` を返す (台帳 FX-1)。
- 同じ初期状態と同じ入力列からは、同じ返答、同じ `Effect` の列、同じ状態になる。走査は ID の
  スロット順か登録順で行う。検査: `kome-testkit` の `the_same_inputs_give_the_same_state_and_effects`。

## 単一 writer と実行権

一つの `Kernel` を更新する実行主体は一つである。ゲスト命令の実行とメモリアクセスは worker が
行い、kome はそのための実行権 (`RunPermit`) を貸す。

- 実行権は複製できず、kome の外では作れない。返すと値が消費される。検査: `sched.rs` の
  compile_fail 試験 (lint `compile-fail-kept` が数を保つ)。
- 返却を受理する前に拒否した実行権 (別の実行インスタンスのもの、古いもの) は消費せず、失敗の値
  (`NotRelinquished::Rejected`) に入れて返す。届け先を間違えても実行権は失われない。受理した後の
  内部の障害 (`NotRelinquished::Failed`) とは型で区別する。検査: lint `permit-returned`。
- 実行権は、実行インスタンス、スレッド、プロセス、worker、実行の世代、アドレス空間の世代、
  実行してよい量を持つ。kome も貸し出しを記録し、返却のときに照合する (台帳 RUN-1)。
- 実行量の単位は、ゲスト命令数のように最適化で変わらないものを実行器が選ぶ。

実行器が止まる理由と、kome への伝え方は次のとおり。kome が区別するのは「スレッドが続きを
そのまま実行できるか」だけで、それ以外の区別は Personality が解釈して次の操作を選ぶ。

| 実行器が止まった理由 | 実行権の返し方 | その後に呼ぶ操作 |
| --- | --- | --- |
| 実行量を使い切った、中断の要求に応じた | `Stop::Preempted` | なし (実行待ちへ戻る) |
| API 呼び出し | `Stop::Trapped` | 処理の後 `continue_thread` |
| 待機 | `Stop::Trapped` | `wait`。すぐ確定しなければ結果は `Effect::WaitDone` |
| 例外 | `Stop::Trapped` | Personality が配送を決め、`continue_thread` か `terminate_thread` |
| 終了 | `Stop::Trapped` | `terminate_thread` または `exit_process` |

実行中のスレッドへの終了要求は、そのスレッドが実行権を返したときに確定する。

## 写しと復元

`Kernel` は複製できない。同じ実行インスタンスの状態が二つあると、一つの実行権や完了通知を
両方が受け取れてしまうからである。

- `Kernel::snapshot` は写しを作る。写しは比べることと復元することだけができる。
- `Snapshot::restore` は、写しを別の実行インスタンスの状態として復元する。写しと同じ識別、
  実行権を貸したままの写し、応答待ちの I/O がある写しは拒否する。復元前の実行権と完了通知は、
  復元した状態では `Foreign` になる。
- 型つき ID そのものは実行インスタンスの識別を持たない。復元した状態では、写しを取った時点の
  ID がそのまま同じ資源を指す。無関係な二つの `Kernel` の ID を取り違えないことは、
  Personality が実行インスタンスごとに表を分けて守る。

検査: `tests.rs` の `a_snapshot_is_restored_only_as_another_instance_with_nothing_lent`、
`kernel.rs` の compile_fail 試験、決定的シミュレーションの復元への乗り換え。

## 規則と検査の対応

| 規則 | 検査 |
| --- | --- |
| kome が OS・ホストへ依存しない | lint `no-deps`、`no-std`、`host-vocabulary`、OS の無いターゲットのビルド |
| 混同してはいけない値を別の型にする | 型 (`GuestAddr`、`BackingOffset`、`Id<K>`)、compile_fail 試験 |
| 状態管理へ `unsafe` が広がらない | `unsafe_code = "forbid"` (lint `required-lints`) |
| 状態の追加を既存の処理が見落とさない | lint `no-catch-all` |
| 新しい遷移の検証が抜けない | lint `ledger`、`operations-covered`、`tla-listed` |
| 走らなかった試験が合格に混ざらない | `scripts/ci/test.sh` の照合、lint `no-skipped-tests` |
| 確保しないと約束した操作が確保しない | `checks/noalloc` (`docs/memory.md`) |
| その場で検査を黙らせない | lint `no-local-allow`、`escape-comments`、`ci-required` |
