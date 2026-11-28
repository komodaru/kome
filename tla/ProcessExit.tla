---------------------------- MODULE ProcessExit ----------------------------
(* 一つのプロセスの終了処理: 終了の要求、実行中のスレッドの中断と返却、参照の解放、
   終了の通知、応答待ちの I/O の終端、回収。

   カーネルの動きは ExitTable (crates/kome の Kernel を動かして生成) と IoTable をそのまま使う。
   ここが加えるのは周りの動き: worker (実行権を受け取り、返す)、Personality (待機、再開、
   I/O の発行と取消、スレッドとプロセスの終了の要求)、ホスト (I/O の応答)。

   モデルの変数と実装の対応 (scripts/ci/tla.sh が、状態と遷移の全体を実際の Kernel の動きと
   突き合わせる。docs/tla-conformance.md):
     run      Thread::run (Gone は表から消えたこと)
     kill     Thread::kill が Some であること
     asked    Run::Running の interrupted (Effect::Interrupt を出したこと)
     life     Process::life (Reclaimed は表から消えたこと)
     io       Io::state (None は要求が無いこと、Gone は回収したこと)
     handles  Process::objects と Process::backings の参照の数
     exits    Effect::ProcessExited を出した回数

   確かめる性質:
     NoExitWhileRunning    実行中のスレッドが残る間は、終了を通知しない。
     NoGrantAfterExit      終了を要求したプロセスのスレッドへ、新しい実行権を貸さない。
     ReleasedAtExit        終了を通知した時点で、プロセスの参照は残っていない。
     ExitsOnce             終了の通知は一度だけ。
     NoReclaimInFlight     応答待ちの I/O が残る間は、プロセスを回収しない。
     KilledAreInterrupted  終了を待たせている実行中のスレッドには、中断を要求してある。
     ExitFinishes          worker が中断に応じれば、終了の要求は必ず終了の通知まで進む。
     Reclaims              ホストが応答すれば、終了したプロセスは必ず回収される。

   定数は、守るべき規則を一つずつ外した変種を作る。変種では反例が出なければならない
   (scripts/ci/tla.sh)。 *)
EXTENDS ExitTable, IoTable, Naturals, FiniteSets

CONSTANTS
    Threads,
    FinishWhileRunning,  \* TRUE: 終了の要求の時点で、実行中のスレッドを待たずに終了を通知する (誤り)
    KeepReady,           \* TRUE: 終了の要求が、実行待ちのスレッドを残す (誤り)
    KeepHandles,         \* TRUE: 終了の通知のときに参照を手放さない (誤り)
    ReclaimWithIo,       \* TRUE: 応答待ちの I/O があっても回収する (誤り)
    AcceptExitTwice,     \* TRUE: 終了処理に入ったプロセスへの終了の要求を受理する (誤り)
    NoInterrupt          \* TRUE: 終了を求めた実行中のスレッドへ中断を要求しない (誤り)

VARIABLES
    run,       \* スレッドの状態
    kill,      \* 実行権を返したら終了するか
    asked,     \* この実行に中断を要求したか
    life,      \* プロセスの段階
    io,        \* プロセスの I/O 要求の状態
    handles,   \* プロセスが持つ参照の数
    exits,     \* 終了を通知した回数
    lateGrant, \* 終了の要求の後に実行権を貸したか
    drained    \* 終了の通知の後に I/O の応答を受けたか

vars == <<run, kill, asked, life, io, handles, exits, lateGrant, drained>>

RunStates == {"Ready", "Running", "Servicing", "Blocked", "Gone"}
InFlight == {"Issued", "CancelRequested"}
Over == {"Draining", "Reclaimed"}

TypeOK ==
    /\ run \in [Threads -> RunStates]
    /\ kill \in [Threads -> BOOLEAN]
    /\ asked \in [Threads -> BOOLEAN]
    /\ life \in {"Alive", "Exiting"} \cup Over
    /\ io \in IoStates \cup {"None", "Gone"}
    /\ handles \in 0..1
    /\ exits \in 0..3
    /\ lateGrant \in BOOLEAN /\ drained \in BOOLEAN

Init ==
    /\ run = [t \in Threads |-> "Ready"]
    /\ kill = [t \in Threads |-> FALSE]
    /\ asked = [t \in Threads |-> FALSE]
    /\ life = "Alive" /\ io = "None" /\ handles = 1 /\ exits = 0
    /\ lateGrant = FALSE /\ drained = FALSE

NoneLeft(r) == \A t \in Threads : r[t] = "Gone"

\* 最後のスレッドが止まった: 参照を手放し、終了を通知する。I/O と段階は生成した表のとおり。
Finish ==
    LET row == ExitFinish[io]
    IN /\ life' = IF ReclaimWithIo THEN "Reclaimed" ELSE row[1]
       /\ io' = row[2]
       /\ handles' = IF KeepHandles THEN handles ELSE 0
       /\ exits' = exits + 1

\* スレッドの状態が r になった。requested はプロセスの終了を要求したかどうか。
Settle(r, requested) ==
    /\ run' = r
    /\ IF /\ NoneLeft(r) \/ (FinishWhileRunning /\ requested)
          /\ life \in {"Alive", "Exiting"} \/ AcceptExitTwice
       THEN Finish
       ELSE /\ life' = IF requested /\ life = "Alive" THEN "Exiting" ELSE life
            /\ UNCHANGED <<io, handles, exits>>

\* ---- worker ----
Grant(t) ==
    /\ run[t] = "Ready" /\ life \in {"Alive", "Exiting"}
    /\ run' = [run EXCEPT ![t] = "Running"]
    /\ lateGrant' = (lateGrant \/ life # "Alive")
    /\ UNCHANGED <<kill, asked, life, io, handles, exits, drained>>

\* 終了を求められていないスレッドが実行権を返す。
Return(t) ==
    /\ run[t] = "Running" /\ ~kill[t]
    /\ \E next \in {"Ready", "Servicing"} : run' = [run EXCEPT ![t] = next]
    /\ asked' = [asked EXCEPT ![t] = FALSE]
    /\ UNCHANGED <<kill, life, io, handles, exits, lateGrant, drained>>

\* 終了を求められていないスレッドが、自分から終了して実行権を返す。
Leave(t) ==
    /\ run[t] = "Running" /\ ~kill[t]
    /\ asked' = [asked EXCEPT ![t] = FALSE]
    /\ Settle([run EXCEPT ![t] = "Gone"], FALSE)
    /\ UNCHANGED <<kill, lateGrant, drained>>

\* 終了を求められたスレッドが実行権を返す。どの理由で返しても終了する。
KilledReturn(t) ==
    /\ run[t] = "Running" /\ kill[t]
    /\ kill' = [kill EXCEPT ![t] = FALSE]
    /\ asked' = [asked EXCEPT ![t] = FALSE]
    /\ Settle([run EXCEPT ![t] = "Gone"], FALSE)
    /\ UNCHANGED <<lateGrant, drained>>

\* ---- Personality ----
Move(t, from, to) ==
    /\ life = "Alive" /\ run[t] = from
    /\ run' = [run EXCEPT ![t] = to]
    /\ UNCHANGED <<kill, asked, life, io, handles, exits, lateGrant, drained>>

Continue(t) == Move(t, "Servicing", "Ready")
Block(t) == Move(t, "Servicing", "Blocked")
\* 待機が終わったスレッドは、サービス中に戻る。実行待ちにするのは Personality (Continue)。
Wake(t) == Move(t, "Blocked", "Servicing")

Serviced == \E t \in Threads : run[t] = "Servicing"

IoPrepare ==
    /\ life = "Alive" /\ Serviced /\ io = "None"
    /\ io' = "Prepared"
    /\ UNCHANGED <<run, kill, asked, life, handles, exits, lateGrant, drained>>

IoDo(e) ==
    /\ life = "Alive" /\ Serviced /\ <<io, e>> \in DOMAIN IoStep
    /\ io' = IoStep[<<io, e>>][1]
    /\ UNCHANGED <<run, kill, asked, life, handles, exits, lateGrant, drained>>

IoRelease ==
    /\ life = "Alive" /\ Serviced /\ io \in {"Prepared", "Settled", "Published"}
    /\ io' = "None"
    /\ UNCHANGED <<run, kill, asked, life, handles, exits, lateGrant, drained>>

Terminate(t) ==
    /\ life = "Alive" /\ run[t] # "Gone"
    /\ LET row == ExitThread[<<"Terminate", run[t]>>]
       IN /\ kill' = [kill EXCEPT ![t] = (row[1] = "Running")]
          /\ asked' = [asked EXCEPT ![t] = (asked[t] \/ (row[2] /\ ~NoInterrupt))]
          /\ Settle([run EXCEPT ![t] = row[1]], FALSE)
    /\ UNCHANGED <<lateGrant, drained>>

ExitProcess ==
    /\ life = "Alive" \/ (AcceptExitTwice /\ life \in {"Exiting", "Draining"})
    /\ LET after(t) == IF run[t] = "Gone" \/ (KeepReady /\ run[t] = "Ready")
                       THEN <<run[t], FALSE>>
                       ELSE ExitThread[<<"ExitProcess", run[t]>>]
           r == [t \in Threads |-> after(t)[1]]
       IN /\ kill' = [t \in Threads |-> kill[t] \/ r[t] = "Running"]
          /\ asked' = [t \in Threads |-> asked[t] \/ (after(t)[2] /\ ~NoInterrupt)]
          /\ Settle(r, TRUE)
    /\ UNCHANGED <<lateGrant, drained>>

\* ---- ホスト ----
HostComplete ==
    /\ io \in InFlight /\ life # "Reclaimed"
    /\ IF life = "Draining"
       THEN /\ io \in DOMAIN ExitComplete
            /\ life' = ExitComplete[io][1]
            /\ io' = ExitComplete[io][2]
            /\ drained' = TRUE
       ELSE /\ io' = IoStep[<<io, "Complete">>][1]
            /\ UNCHANGED <<life, drained>>
    /\ UNCHANGED <<run, kill, asked, handles, exits, lateGrant>>

Next ==
    \/ \E t \in Threads : Grant(t) \/ Return(t) \/ Leave(t) \/ KilledReturn(t)
    \/ \E t \in Threads : Continue(t) \/ Block(t) \/ Wake(t) \/ Terminate(t)
    \/ IoPrepare \/ IoRelease \/ \E e \in {"Issue", "Cancel", "Publish"} : IoDo(e)
    \/ ExitProcess \/ HostComplete

\* worker は、中断を要求された実行権を必ず返す。ホストは必ず応答する。
Spec ==
    /\ Init /\ [][Next]_vars
    /\ \A t \in Threads : WF_vars(asked[t] /\ KilledReturn(t))
    /\ WF_vars(HostComplete)

\* ---- 性質 ----
NoExitWhileRunning == life \in Over => NoneLeft(run)

NoGrantAfterExit == ~lateGrant

ReleasedAtExit == life \in Over => handles = 0

ExitsOnce == exits <= 1 /\ ((exits = 1) <=> (life \in Over))

NoReclaimInFlight == life = "Reclaimed" => io \notin InFlight

KilledAreInterrupted == \A t \in Threads : kill[t] => (run[t] = "Running" /\ asked[t])

ExitFinishes == (life = "Exiting") ~> (life \in Over)

Reclaims == (life = "Draining") ~> (life = "Reclaimed")

\* 到達確認: 実行中のスレッドを待ち、応答待ちの I/O の終端も待ってから回収される実行がある
\* (違反として検出されるのが正しい)。
ReachDrained == ~(life = "Reclaimed" /\ io = "Gone" /\ drained /\ exits = 1 /\ handles = 0)
=============================================================================
