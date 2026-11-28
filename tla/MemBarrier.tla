---------------------------- MODULE MemBarrier ----------------------------
(* 権限を減らすメモリの変更と、実行中のスレッド: 古い世代の実行、貸し出しの停止、実体の猶予と回収。

   プロセス P にスレッドが二つ、別のプロセス Q に一つ。P は実体を一つ持ち、一か所に対応づけて
   いる。P のスレッドが実行中に、対応づけを外すか権限を狭めると、その実行は変更の前の見え方
   (古い世代) のまま走っている。カーネルは、その実行へ中断を要求し、全て戻るまで P のスレッドへ
   新しい実行権を貸さず、外した実体を解放しない。

   モデルの変数と実装の対応 (scripts/ci/tla.sh が、状態と遷移の全体を実際の Kernel の動きと
   突き合わせる。docs/tla-conformance.md):
     run         Thread::run
     stale       Run::Running の stale (古い世代の実行)
     asked       Run::Running の interrupted
     map         P の対応づけの権限 (無ければ Off)
     hold        P が実体の保持を持っているか (Process::backings)
     limbo       古い世代の実行が戻るまでの猶予 (Space::limbo)
     backing     実体が表にあるか (回収したら Gone)
     lentBehind  古い世代の実行が残る間に、P のスレッドの実行権を貸したか

   確かめる性質:
     NoLendBehindBarrier  古い世代の実行が残る間は、P のスレッドへ実行権を貸さない。
     NoFreeInUse          古い世代の実行が残る間は、実体を回収しない。
     StaleAreAsked        古い世代の実行には、中断を要求してある。
     LimboOnlyBehind      猶予があるのは、古い世代の実行が残る間だけ。
     NothingLeaks         保持も対応づけも猶予も無い実体は、回収してある。
     BarrierLifts         worker が中断に応じれば、古い世代の実行は必ず無くなる (貸し出しが再開する)。

   定数は、守るべき規則を一つずつ外した変種を作る。変種では反例が出なければならない
   (scripts/ci/tla.sh)。 *)
EXTENDS Naturals, FiniteSets

CONSTANTS
    t1, t2, t3,   \* スレッド。t1 と t2 はプロセス P、t3 はプロセス Q
    LendBehind,   \* TRUE: 古い世代の実行が残っていても貸す (誤り)
    FreeAtOnce,   \* TRUE: 古い世代の実行が残っていても、外した実体に猶予を付けない (誤り)
    NoAsk,        \* TRUE: 古い世代になった実行へ中断を要求しない (誤り)
    KeepLimbo,    \* TRUE: 古い世代の実行が無くなっても、猶予を外さない (誤り)
    ForgetReclaim \* TRUE: 猶予を外したときに、使われなくなった実体を回収しない (誤り)

Threads == {t1, t2, t3}
Inside == {t1, t2}

VARIABLES
    run,       \* スレッドの状態
    stale,     \* 古い世代の実行か
    asked,     \* この実行に中断を要求したか
    map,       \* P の対応づけ
    hold,      \* P が実体の保持を持っているか
    limbo,     \* 古い世代の実行が戻るまでの猶予があるか
    backing,   \* 実体
    lentBehind \* 古い世代の実行が残る間に、P のスレッドの実行権を貸したか

vars == <<run, stale, asked, map, hold, limbo, backing, lentBehind>>

TypeOK ==
    /\ run \in [Threads -> {"Ready", "Running", "Servicing"}]
    /\ stale \in [Threads -> BOOLEAN]
    /\ asked \in [Threads -> BOOLEAN]
    /\ map \in {"ReadWrite", "Read", "Off"}
    /\ hold \in BOOLEAN /\ limbo \in BOOLEAN
    /\ backing \in {"Live", "Gone"}
    /\ lentBehind \in BOOLEAN

Init ==
    /\ run = [t \in Threads |-> "Ready"]
    /\ stale = [t \in Threads |-> FALSE]
    /\ asked = [t \in Threads |-> FALSE]
    /\ map = "ReadWrite" /\ hold = TRUE /\ limbo = FALSE /\ backing = "Live"
    /\ lentBehind = FALSE

Barred(s) == \E t \in Inside : s[t]

\* 保持も対応づけも猶予も無い実体は、回収する。
After(h, m, l) == IF ~h /\ m = "Off" /\ ~l THEN "Gone" ELSE backing

\* ---- worker ----
Grant(t) ==
    /\ run[t] = "Ready"
    /\ LendBehind \/ t \notin Inside \/ ~Barred(stale)
    /\ run' = [run EXCEPT ![t] = "Running"]
    /\ lentBehind' = (lentBehind \/ (t \in Inside /\ Barred(stale)))
    /\ UNCHANGED <<stale, asked, map, hold, limbo, backing>>

\* 実行権を返す。古い世代の実行が無くなったら、猶予を外し、使われなくなった実体を回収する。
Return(t, next) ==
    /\ run[t] = "Running"
    /\ run' = [run EXCEPT ![t] = next]
    /\ stale' = [stale EXCEPT ![t] = FALSE]
    /\ asked' = [asked EXCEPT ![t] = FALSE]
    /\ limbo' = (limbo /\ (Barred(stale') \/ KeepLimbo))
    /\ backing' = IF ForgetReclaim THEN backing ELSE After(hold, map, limbo')
    /\ UNCHANGED <<map, hold, lentBehind>>

\* ---- Personality ----
Continue(t) ==
    /\ run[t] = "Servicing"
    /\ run' = [run EXCEPT ![t] = "Ready"]
    /\ UNCHANGED <<stale, asked, map, hold, limbo, backing, lentBehind>>

\* 権限を減らす変更: いま実行中の P のスレッドは古い世代になり、中断を要求される。
Reduce ==
    /\ stale' = [t \in Threads |-> stale[t] \/ (t \in Inside /\ run[t] = "Running")]
    /\ asked' = [t \in Threads |-> asked[t] \/ (~NoAsk /\ t \in Inside /\ run[t] = "Running")]

Unmap ==
    /\ map # "Off"
    /\ Reduce
    /\ map' = "Off"
    /\ limbo' = (limbo \/ (Barred(stale') /\ ~FreeAtOnce))
    /\ backing' = After(hold, "Off", limbo')
    /\ UNCHANGED <<run, hold, lentBehind>>

Narrow ==
    /\ map = "ReadWrite"
    /\ Reduce
    /\ map' = "Read"
    /\ UNCHANGED <<run, hold, limbo, backing, lentBehind>>

\* 権限を増やすだけの変更は、実行中のスレッドを止めない。
Widen ==
    /\ map = "Read"
    /\ map' = "ReadWrite"
    /\ UNCHANGED <<run, stale, asked, hold, limbo, backing, lentBehind>>

\* 外した実体を、もう一度対応づける。
Map ==
    /\ map = "Off" /\ backing = "Live"
    /\ map' = "ReadWrite"
    /\ UNCHANGED <<run, stale, asked, hold, limbo, backing, lentBehind>>

Release ==
    /\ hold /\ backing = "Live"
    /\ hold' = FALSE
    /\ backing' = After(FALSE, map, limbo)
    /\ UNCHANGED <<run, stale, asked, map, limbo, lentBehind>>

Next ==
    \/ \E t \in Threads : Grant(t) \/ Return(t, "Ready") \/ Return(t, "Servicing") \/ Continue(t)
    \/ Unmap \/ Narrow \/ Widen \/ Map \/ Release

\* worker は、中断を要求された実行権を必ず返す。
Spec ==
    /\ Init /\ [][Next]_vars
    /\ \A t \in Threads : WF_vars(asked[t] /\ (Return(t, "Ready") \/ Return(t, "Servicing")))

\* ---- 性質 ----
NoLendBehindBarrier == ~lentBehind

NoFreeInUse == backing = "Gone" => ~Barred(stale)

StaleAreAsked == \A t \in Threads : stale[t] => (t \in Inside /\ run[t] = "Running" /\ asked[t])

LimboOnlyBehind == limbo => Barred(stale)

NothingLeaks == (~hold /\ map = "Off" /\ ~limbo) => backing = "Gone"

BarrierLifts == Barred(stale) ~> ~Barred(stale)

\* 到達確認: P の古い世代の実行が残り、実体が猶予だけで生きている間に、Q のスレッドは実行中に
\* なれる (違反として検出されるのが正しい)。
ReachOthersRun ==
    ~(Barred(stale) /\ limbo /\ ~hold /\ map = "Off" /\ backing = "Live" /\ run[t3] = "Running")
=============================================================================
