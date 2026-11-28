# TLA+ のモデルと実装の突き合わせ

`tla/` のモデルは設計を調べる。モデルが正しくても、Rust の実装がモデルと違う動きをしていれば、
調べた性質は実装について何も言わない。ここでは、モデルの状態と遷移の全体が、実際の `Kernel` の
動きと過不足なく一致することを機械的に確かめる。

## 確かめ方

1. `scripts/ci/tla.sh` が、正しい設定のモデルを TLC で検査するときに、状態グラフ (到達できる全ての
   状態と、その間の全ての遷移) を書き出させる。
2. `crates/kome/src/conform.rs` の試験が、実際の `Kernel` を初期状態から幅優先で動かす。モデルの
   動きに当たる操作を全て試し、操作のたびに `Kernel` の状態をモデルの変数の値へ写す。
3. 次の二つを確かめる。
   - 実装が行った遷移は、全てモデルにある。モデルに無い状態や遷移に出会ったら、そこまでの操作の
     列を出して止まる。
   - モデルの遷移は、全て実装が行った。
4. 突き合わせ自身が違いを見分けることも毎回確かめる。モデルの遷移を一つ抜いたグラフと、一つ
   足したグラフは、どちらも一致しないと判定されなければならない。

`tla.sh` は、四つのモデルそれぞれについて「一致した」行が出たことまで見る。試験が走らなかった
場合も失敗になる。

| モデル | 状態 | 遷移 |
| --- | --- | --- |
| `IoLifecycle` | 86 | 194 |
| `WaitRace` | 67581 | 228420 |
| `ProcessExit` | 1336 | 10257 |
| `RunPermits` | 3296 | 27480 |

## モデルの変数と実装の値

「外の世界」と書いた変数は、カーネルの外 (ホストと Personality) の状態で、実装に対応する値が無い。
試験が同じ規則で自分で持つ。「履歴」と書いた変数は、性質を書くためにモデルが覚えている過去で、
試験がカーネルの返答と `Effect` から数える。

### WaitRace (待機と通知の競合)

スレッド三つ、数の上限が 2 の資源二つ (`SignalMode::Consume`)。

| モデル | 実装 |
| --- | --- |
| `count[o]` | `Object::count` (`Kernel::signal_count`) |
| `phase[t]` | `Thread::run` が `Blocked` なら `waiting`。`Servicing` で、待機を始める前なら `idle`、後なら `done` |
| `want[t]` | `Run::Blocked` が持つ `Wait::targets` と `Wait::mode` (待機中は一致を確かめる) |
| `queue` | 待機を登録した順。資源ごとに抜き出した列が `Object::waiters` と一致することを確かめる |
| `outcomes[t]` | 履歴。`WaitStart::Done` と `Effect::WaitDone` の数 |
| `took[t]` | 履歴。確定した `WaitOutcome` が指す対象 |
| `sent[o]` | 履歴。受理された `Kernel::signal` の数 |

| モデルの動き | 実装の操作 |
| --- | --- |
| `Wait(t, w)` | `Kernel::wait` |
| `Signal(o)` | `Kernel::signal(o, Add(1))` |
| `Timeout(t)` | `Kernel::advance` で、その待機の期限の時刻へ進める |

期限は待機を始めるときに決まるので、試験は「この後 k 番目に期限が来る待機」の期限を時刻 k にして
始める。どの待機の期限も、どの順でも来る。モデルの `MaxSignals` (通知の回数の上限) は探索を
有限にするためのもので、試験も同じ回数で止める。

### IoLifecycle (一つの I/O 要求の寿命)

プロセス一つ、通知先の資源を待つスレッド一つ、I/O 要求一つ。

| モデル | 実装 |
| --- | --- |
| `state` | `Io::state` (`Kernel::io_phase`)。表から消えていれば `Gone` |
| `owner` | `Process::life` が `Alive` なら `alive` |
| `woken` | 待っているスレッドへの `Effect::WaitDone` が出たか |
| `effects` | `Effect::IoSubmit` と `Effect::IoCancel` のうち、ホストがまだ受け取っていないもの |
| `settled` | 履歴。終端が確定した回数 (`Kernel::complete` の成功と、`CancelReply::Withdrawn`) |
| `host`、`msgs`、`resent` | 外の世界 (ホスト) |
| `written` | 外の世界 (Personality が出力を書いたか) |

| モデルの動き | 実装の操作 |
| --- | --- |
| `Issue`、`Cancel`、`Publish`、`Release` | `issue_io`、`cancel_io`、`publish_io`、`release_io` |
| `Deliver` | `Kernel::complete` |
| `OwnerExit` | `Kernel::exit_process` |
| `WriteOutput`、`HostTake`、`HostSeeCancel`、`HostAnswer`、`HostResend` | 外の世界だけが変わる |

操作を受け付けるかどうかは、カーネルの返答で決める (拒否なら、その動きは無い)。試験が自分で
決めるのは外の世界の規則だけである: 出力を書くのは終端が確定した後、公開するのは出力を書いた後。

### ProcessExit (プロセスの終了処理)

プロセス一つ、スレッド三つ、worker 三つ、I/O 要求は一度に一つ。

| モデル | 実装 |
| --- | --- |
| `run[t]` | `Thread::run`。表から消えていれば `Gone` |
| `kill[t]` | `Thread::kill` が `Some` |
| `asked[t]` | `Run::Running` の `interrupted` |
| `life` | `Process::life`。表から消えていれば `Reclaimed` |
| `io` | `Io::state`。要求が無ければ `None`、プロセスの終了で回収されていれば `Gone` |
| `handles` | `Process::objects` と `Process::backings` に参照が残っているか |
| `exits` | 履歴。`Effect::ProcessExited` の数 |
| `lateGrant` | 履歴。`Process::life` が `Alive` でないときに `grant_thread` が成功したか |
| `drained` | 履歴。`Kernel::complete` が `IoSettled::Orphaned` を返したか |

| モデルの動き | 実装の操作 |
| --- | --- |
| `Grant(t)` | `Kernel::grant_thread` |
| `Return(t)`、`Leave(t)`、`KilledReturn(t)` | `Kernel::relinquish` (理由は `Yielded`、`BudgetExhausted`、`Interrupted`、`Trap`、`Exited`) |
| `Continue(t)`、`Block(t)`、`Wake(t)` | `continue_thread`、`wait`、待っている資源への `signal` |
| `Terminate(t)`、`ExitProcess` | `terminate_thread`、`exit_process` |
| `IoPrepare`、`IoDo(e)`、`IoRelease` | `prepare_io`、`issue_io`、`cancel_io`、`publish_io`、`release_io` |
| `HostComplete` | `Kernel::complete` |

モデルは「Personality が I/O を扱うのは、サービス中のスレッドがあるときだけ」としている。これは
カーネルの規則ではないので、試験も同じ条件で I/O の操作を試す。

### RunPermits (複数の worker への実行権の貸し出し)

プロセス一つ、スレッド三つ (先に作った一つは優先度が低く、後の二つは高い)、worker 三つ、同時に
貸せる実行権は二つ。

| モデル | 実装 |
| --- | --- |
| `run[t]` | `Thread::run` |
| `on[t]` | `Run::Running` の `worker`。実行中でなければ `None` |
| `sent[t]` | 履歴。いまの実行について出た `Effect::Interrupt` の数。`Run::Running` の `interrupted` と合うことを確かめる |
| `suspend[t]` | `Thread::suspend` |
| `queue` | `Kernel::ready` の席を、実行待ちになった順 (`Waiting::turn`) に並べたもの。各スレッドが持つ位置と、先頭の席も確かめる |
| `lentSuspended` | 履歴。停止要求があるスレッドの実行権を貸したか |
| `overtaken` | 履歴。`Kernel::grant` が、貸す前の列の中で先に貸すべきスレッドを飛ばしたか (実装の比較を使わずに数える) |

| モデルの動き | 実装の操作 |
| --- | --- |
| `Grant(w)`、`GrantThread(w, t)` | `Kernel::grant`、`Kernel::grant_thread` |
| `Return(t, next)` | `Kernel::relinquish` (理由は `Yielded`、`BudgetExhausted`、`Interrupted`、`Trap`) |
| `Continue(t)`、`Preempt(t)` | `continue_thread`、`preempt_thread` |
| `Suspend(t)`、`Unsuspend(t)` | `suspend_thread`、`unsuspend_thread` |

モデルの `MaxSuspend` (停止要求の数の上限) は探索を有限にするためのもので、試験も同じ数で止める。
`Stop::Interrupted` で返せるのは中断を要求した実行だけであることは、返した後の状態が他の理由と
同じなのでグラフには現れない。試験が、受理と拒否のそれぞれで `sent` と合うことを確かめる。
出た `Effect::Interrupt` が、いまの実行 (worker と実行の番号) を指すことも確かめる。

実装の規則を一つずつ書き換えて、この突き合わせだけを走らせた (2026-11-28、11 件): 貸す順序の
比較の二つの向き、実行中のスレッドへの停止要求での中断、停止要求の解除と列、停止要求での列からの
取り出し、返却と再開での列への追加、worker が持つ実行権の検査、数の上限、名指しの貸し出しの
停止要求の検査、要求していない中断での返却。11 件とも落ちた。

## この突き合わせで見つけて直したモデルの食い違い

- `ProcessExit` の `Wake` は、待機が終わったスレッドを実行待ち (`Ready`) にしていた。実装は
  サービス中 (`Servicing`) に戻し、実行待ちにするのは Personality の `continue_thread` である。
  モデルを実装に合わせた。
- `ProcessExit` に、スレッドが自分から終了して実行権を返す動き (`Stop::Exited`) が無かった。
  `Leave(t)` を足した。最後のスレッドがこの動きで居なくなったときも、プロセスの終了の通知まで進む。

どちらを直した後も、モデルの性質は全て成り立ち、誤りの変種は名指しした性質の反例を出す。

## 実装を壊すと、突き合わせが落ちること

`cargo-mutants` で `wait.rs`、`io.rs`、`process.rs`、`object.rs` を一箇所ずつ書き換え、この
突き合わせの試験だけを走らせた結果を、下に記録する (他の試験は走らせていない)。

| | 件数 |
| --- | --- |
| 書き換えてもコンパイルできたもの | 117 |
| 突き合わせが落ちたもの | 70 |
| 落ちなかったもの | 47 |

2026-11-28 の測定。落ちなかった 47 件は、どれもモデルに無い操作か値の書き換えである: 値を読むだけの
関数 (`io_outcome`、`thread_count`、`ExitCode::get` など)、資源を作るときの引数の検査、プロセスごとの
割り当て、参照の追加と解放 (`retain_object`、`release_object`)、待機の中断 (`interrupt_wait`)、
対象の数の上限、過去の期限での待機、数の置き換え (`SignalOp::Set`)、検査 (`audit`) の中の比較、
起こらないはずの状態への備え。これらは他の試験が見る (週ごとの変異試験は、全ての試験で調べる)。
待機の成立と消費、登録順の評価、期限、I/O の遷移、終了処理の書き換えは、全て落ちた。

## 限界

- モデルの一つの状態につき、実装の状態は最初に辿り着いた一つだけを代表として調べる。同じモデルの
  状態に写る別の実装の状態 (時刻、ID の世代、実行した量など、モデルに無い値が違うもの) から先の
  動きは、ここでは見ていない。そちらは決定的シミュレーション (`kome-testkit`) と `Kernel::audit` が見る。
- 突き合わせるのは、モデルの定数で決まる大きさ (スレッド三つ、worker 三つ、資源二つ、I/O 要求一つ) までである。
- モデルに無い操作 (待機の中断、優先度の変更、メモリ、タイマー、通信路) は対象にならない。
- 活性 (`Finishes`、`ExitFinishes`、`Reclaims`、`Progress`) はモデルの上で TLC が調べる。実装については、
  遷移が一致することから従う範囲でしか言えない (公平性の仮定は、ホストと worker の振る舞いである)。
