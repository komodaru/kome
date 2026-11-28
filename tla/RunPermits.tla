---------------------------- MODULE RunPermits ----------------------------
(* 複数の worker への実行権の貸し出し: 実行待ちの列、貸し出し、返却、中断の要求、停止要求。

   スレッドは実行待ち・実行中・サービス中のどれかにある。実行権を貸す入口は二つある:
   順序に従う Grant (優先度が高い方、同じなら先に実行待ちになった方) と、ホストが名指しする
   GrantThread (順序は見ないが、他の規則は同じ)。

   モデルの変数と実装の対応 (scripts/ci/tla.sh が、状態と遷移の全体を実際の Kernel の動きと
   突き合わせる。docs/tla-conformance.md):
     run            Thread::run
     on             Run::Running の worker (実行中でなければ None)
     sent           この実行について出した Effect::Interrupt の数
     suspend        Thread::suspend
     queue          Kernel::ready の席を、実行待ちになった順 (Waiting::turn) に並べたもの
     lentSuspended  停止要求があるスレッドの実行権を貸したか
     overtaken      順序に従う貸し出しが、先に貸すべきスレッドを飛ばしたか

   確かめる性質:
     OneWorkerOnePermit    一つの worker が持つ実行権は一つ。
     WithinLimit           貸している実行権の数は、worker の数の上限を超えない。
     LentWhileRunning      実行権が出ているのは、スレッドが実行中の間だけ。
     QueueExact            列は、いま貸せるスレッド (実行待ちで、停止要求が無い) をちょうど一度ずつ含む。
     NoLendWhileSuspended  停止要求があるスレッドの実行権は貸さない。
     InterruptOnce         一度の実行につき、中断の要求は一度だけ出る。
     SuspendedRunnerAsked  停止要求がある実行中のスレッドには、中断を要求してある。
     NoOvertake            順序に従う貸し出しは、先に貸すべきスレッドを飛ばさない。
     Progress              worker が実行権を返し、順序に従う貸し出しが繰り返されるなら、最も高い
                           優先度で列にあるスレッドは、必ず実行権を受け取る (停止要求で列を出る場合を除く)。
                           優先度が低いスレッドには、この保証は無い (高い方が実行待ちに戻り続ければ待ち続ける)。

   定数は、守るべき規則を一つずつ外した変種を作る。変種では反例が出なければならない
   (scripts/ci/tla.sh)。 *)
EXTENDS Naturals, Sequences, FiniteSets

CONSTANTS
    t1, t2, t3,          \* スレッド。この順に作る
    Workers,
    None,                \* 実行権が出ていないことを表す値
    Limit,               \* 同時に貸せる実行権の数 (Limits::workers)
    MaxSuspend,          \* 停止要求の数の上限 (探索を有限にするためのもの)
    LendToBusyWorker,    \* TRUE: 実行権を持っている worker へも貸す (誤り)
    OverLimit,           \* TRUE: worker の数の上限を見ない (誤り)
    KeepPermit,          \* TRUE: 返した実行権を、出ているままにする (誤り)
    KeepSuspendedQueued, \* TRUE: 停止要求を受けた実行待ちのスレッドを列に残す (誤り)
    RequeueEarly,        \* TRUE: 停止要求が残っていても、一つ減らしたら列へ入れる (誤り)
    AskAgain,            \* TRUE: 中断を要求済みの実行へ、もう一度要求を出す (誤り)
    NoInterruptOnSuspend,\* TRUE: 実行中のスレッドへの停止要求で、中断を要求しない (誤り)
    Lifo                 \* TRUE: 順序に従う貸し出しが、最後に実行待ちになったスレッドを選ぶ (誤り)

Threads == {t1, t2, t3}
Order == <<t1, t2, t3>>
\* 優先度が高いスレッド (残りは低い)。先に作ったスレッドが、後から作った二つに譲る形にする。
Top == {t2, t3}

VARIABLES
    run,           \* スレッドの状態
    on,            \* スレッドの実行権を持つ worker
    sent,          \* この実行について出した中断の要求の数
    suspend,       \* 停止要求の数
    queue,         \* いま貸せるスレッドを、実行待ちになった順に並べた列
    lentSuspended, \* 停止要求があるスレッドの実行権を貸したか
    overtaken      \* 順序に従う貸し出しが、先に貸すべきスレッドを飛ばしたか

vars == <<run, on, sent, suspend, queue, lentSuspended, overtaken>>

TypeOK ==
    /\ run \in [Threads -> {"Ready", "Running", "Servicing"}]
    /\ on \in [Threads -> Workers \cup {None}]
    /\ sent \in [Threads -> 0..2]
    /\ suspend \in [Threads -> 0..MaxSuspend]
    /\ queue \in Seq(Threads)
    /\ lentSuspended \in BOOLEAN /\ overtaken \in BOOLEAN

\* 作った順に実行待ちになる。
Init ==
    /\ run = [t \in Threads |-> "Ready"]
    /\ on = [t \in Threads |-> None]
    /\ sent = [t \in Threads |-> 0]
    /\ suspend = [t \in Threads |-> 0]
    /\ queue = Order
    /\ lentSuspended = FALSE /\ overtaken = FALSE

InQueue(q) == {q[i] : i \in DOMAIN q}
Without(q, t) == SelectSeq(q, LAMBDA other : other # t)
Place(q, t) == CHOOSE i \in DOMAIN q : q[i] = t
Priority(t) == IF t \in Top THEN 1 ELSE 0

\* 列 q の中で、a を b より先に貸すか。優先度が高い方、同じなら先に実行待ちになった方。
Ahead(q, a, b) == Priority(a) > Priority(b) \/ (Priority(a) = Priority(b) /\ Place(q, a) < Place(q, b))

\* 列の中で最も先に貸すスレッド。
Best(q) == CHOOSE t \in InQueue(q) : \A other \in InQueue(q) \ {t} : Ahead(q, t, other)

Lent == {t \in Threads : on[t] # None}

\* worker へ貸してよいか: その worker が実行権を持っておらず、数に空きがある。
CanLend(w) ==
    /\ LendToBusyWorker \/ \A t \in Threads : on[t] # w
    /\ OverLimit \/ Cardinality(Lent) < Limit

\* 一度の実行につき、中断の要求は一度だけ出す。
Ask(t) == IF sent[t] = 0 \/ AskAgain THEN [sent EXCEPT ![t] = @ + 1] ELSE sent

Lend(w, t, ordered) ==
    /\ run' = [run EXCEPT ![t] = "Running"]
    /\ on' = [on EXCEPT ![t] = w]
    /\ queue' = Without(queue, t)
    /\ lentSuspended' = (lentSuspended \/ suspend[t] > 0)
    /\ overtaken' = (overtaken \/ (ordered /\ \E other \in InQueue(queue) \ {t} : Ahead(queue, other, t)))
    /\ UNCHANGED <<sent, suspend>>

\* ---- worker ----
\* 順序に従って借りる。
Grant(w) ==
    /\ CanLend(w) /\ queue # <<>>
    /\ Lend(w, IF Lifo THEN queue[Len(queue)] ELSE Best(queue), TRUE)

\* ホストが名指しして貸す。順序は見ない。
GrantThread(w, t) ==
    /\ CanLend(w) /\ t \in InQueue(queue)
    /\ Lend(w, t, FALSE)

\* 実行権を返す。next は返した後の状態: 実行待ち (使い切った、譲った、中断に応じた) か、
\* サービス中 (Personality の処理が要る)。停止要求があるスレッドは列へ入れない。
Return(t, next) ==
    /\ run[t] = "Running"
    /\ run' = [run EXCEPT ![t] = next]
    /\ on' = IF KeepPermit THEN on ELSE [on EXCEPT ![t] = None]
    /\ sent' = [sent EXCEPT ![t] = 0]
    /\ queue' = IF next = "Ready" /\ suspend[t] = 0 THEN Append(queue, t) ELSE queue
    /\ UNCHANGED <<suspend, lentSuspended, overtaken>>

\* ---- Personality ----
Continue(t) ==
    /\ run[t] = "Servicing"
    /\ run' = [run EXCEPT ![t] = "Ready"]
    /\ queue' = IF suspend[t] = 0 THEN Append(queue, t) ELSE queue
    /\ UNCHANGED <<on, sent, suspend, lentSuspended, overtaken>>

Preempt(t) ==
    /\ run[t] = "Running" /\ sent[t] < 2
    /\ sent' = Ask(t)
    /\ UNCHANGED <<run, on, suspend, queue, lentSuspended, overtaken>>

Suspend(t) ==
    /\ suspend[t] < MaxSuspend
    /\ suspend' = [suspend EXCEPT ![t] = @ + 1]
    /\ queue' = IF KeepSuspendedQueued THEN queue ELSE Without(queue, t)
    /\ sent' = IF run[t] = "Running" /\ ~NoInterruptOnSuspend /\ sent[t] < 2 THEN Ask(t) ELSE sent
    /\ UNCHANGED <<run, on, lentSuspended, overtaken>>

Unsuspend(t) ==
    /\ suspend[t] > 0
    /\ suspend' = [suspend EXCEPT ![t] = @ - 1]
    /\ queue' = IF run[t] = "Ready" /\ (suspend[t] = 1 \/ RequeueEarly) THEN Append(queue, t) ELSE queue
    /\ UNCHANGED <<run, on, sent, lentSuspended, overtaken>>

Next ==
    \/ \E w \in Workers : Grant(w) \/ \E t \in Threads : GrantThread(w, t)
    \/ \E t \in Threads : Return(t, "Ready") \/ Return(t, "Servicing")
    \/ \E t \in Threads : Continue(t) \/ Preempt(t) \/ Suspend(t) \/ Unsuspend(t)

\* worker は、借りた実行権を必ず返す (実行量には限りがある)。順序に従う貸し出しは、
\* できる機会が繰り返しあれば、いつかは行われる。
Spec ==
    /\ Init /\ [][Next]_vars
    /\ \A t \in Threads : WF_vars(Return(t, "Ready") \/ Return(t, "Servicing"))
    /\ \A w \in Workers : SF_vars(Grant(w))

\* ---- 性質 ----
OneWorkerOnePermit == \A a, b \in Lent : on[a] = on[b] => a = b

WithinLimit == Cardinality(Lent) <= Limit

LentWhileRunning == \A t \in Threads : (on[t] # None) <=> (run[t] = "Running")

QueueExact ==
    /\ InQueue(queue) = {t \in Threads : run[t] = "Ready" /\ suspend[t] = 0}
    /\ \A i, j \in DOMAIN queue : queue[i] = queue[j] => i = j

NoLendWhileSuspended == ~lentSuspended

InterruptOnce == \A t \in Threads : sent[t] <= 1 /\ (sent[t] > 0 => run[t] = "Running")

SuspendedRunnerAsked == \A t \in Threads : (run[t] = "Running" /\ suspend[t] > 0) => sent[t] > 0

NoOvertake == ~overtaken

Progress == \A t \in Top : (t \in InQueue(queue)) ~> (run[t] = "Running" \/ suspend[t] > 0)

\* 到達確認: 上限まで貸していて、中断を要求済みの実行と、停止要求が重なった実行待ちの
\* スレッドが同時にある (違反として検出されるのが正しい)。
ReachBusy ==
    ~(/\ Cardinality(Lent) = Limit
      /\ \E t \in Threads : sent[t] = 1 /\ suspend[t] = 0
      /\ \E t \in Threads : run[t] = "Ready" /\ suspend[t] = MaxSuspend)
=============================================================================
