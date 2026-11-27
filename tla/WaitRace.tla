------------------------------ MODULE WaitRace ------------------------------
(* 待機と通知の競合: 確認と登録、通知による成立、期限、「すべて」の一括消費。

   成立の判定と消費は WaitTable (実際の Kernel::wait を動かして生成) をそのまま使う。
   ここが加えるのは周りの動き: 複数のスレッドの待機の開始、通知、登録順の評価、期限。

   確かめる性質:
     OneOutcome     一つの待機に結果は一つだけ確定する (台帳 WAIT-1)。
     NoLostWake     成立しているのに待ち続ける待機は無い (台帳 WAIT-2)。
     Conserved      通知した数 = 残っている数 + 消費した数。
     AllOrNothing   「すべて」は全部を一度に取るか何も取らない。「どれか一つ」は一つだけ取る。

   定数は、守るべき規則を一つずつ外した変種を作る。変種では反例が出なければならない
   (scripts/ci/tla.sh)。 *)
EXTENDS WaitTable, Naturals, Sequences, FiniteSets

CONSTANTS
    Threads,
    MaxSignals,     \* 資源ごとの通知の回数の上限 (探索を有限にする)
    SplitCheck,     \* TRUE: 確認と登録を別の手順にする (誤り)
    PartialAll,     \* TRUE: 「すべて」の待機が成立した対象から先に取る (誤り)
    LateTimeout     \* TRUE: 結果が確定した待機にも期限を適用する (誤り)

Objs == {"a", "b"}
Orders == {<<"a">>, <<"b">>, <<"a", "b">>, <<"b", "a">>}
Modes == {"Any", "All"}
None == <<<<>>, "Any">>

VARIABLES
    count,      \* 資源の数
    sent,       \* 資源ごとの、受理した通知の回数
    phase,      \* スレッド: "idle" / "checked" (確認だけ済んだ) / "waiting" / "done"
    want,       \* スレッドの待機 <<対象の列, 条件>>
    queue,      \* 待機中のスレッド。登録順
    outcomes,   \* スレッドの待機に確定した結果の数
    took        \* スレッドが消費した資源

vars == <<count, sent, phase, want, queue, outcomes, took>>

Targets(t) == {want[t][1][i] : i \in 1..Len(want[t][1])}
Decide(c, w) == WaitDecide[<<c["a"], c["b"], w[1], w[2]>>]
Counts(d) == [o \in Objs |-> IF o = "a" THEN d[2] ELSE d[3]]
Taken(c, d) == {o \in Objs : Counts(d)[o] # c[o]}

TypeOK ==
    /\ count \in [Objs -> 0..2]
    /\ sent \in [Objs -> 0..MaxSignals]
    /\ phase \in [Threads -> {"idle", "checked", "waiting", "done"}]
    /\ want \in [Threads -> (Orders \X Modes) \cup {None}]
    /\ outcomes \in [Threads -> 0..2]
    /\ took \in [Threads -> SUBSET Objs]

Init ==
    /\ count = [o \in Objs |-> 0] /\ sent = [o \in Objs |-> 0]
    /\ phase = [t \in Threads |-> "idle"] /\ want = [t \in Threads |-> None]
    /\ queue = <<>> /\ outcomes = [t \in Threads |-> 0] /\ took = [t \in Threads |-> {}]

\* 待機を始める。成立していれば消費して終わる。そうでなければ登録する。
Wait(t, w) ==
    /\ phase[t] = "idle"
    /\ want' = [want EXCEPT ![t] = w]
    /\ LET d == Decide(count, w)
       IN IF d[1] = "Blocked"
          THEN /\ phase' = [phase EXCEPT ![t] = IF SplitCheck THEN "checked" ELSE "waiting"]
               /\ queue' = IF SplitCheck THEN queue ELSE Append(queue, t)
               /\ UNCHANGED <<count, outcomes, took>>
          ELSE /\ phase' = [phase EXCEPT ![t] = "done"]
               /\ count' = Counts(d)
               /\ outcomes' = [outcomes EXCEPT ![t] = @ + 1]
               /\ took' = [took EXCEPT ![t] = Taken(count, d)]
               /\ UNCHANGED queue
    /\ UNCHANGED sent

\* 変種だけ: 確認を済ませたスレッドが、確認し直さずに登録する。
Register(t) ==
    /\ phase[t] = "checked"
    /\ phase' = [phase EXCEPT ![t] = "waiting"]
    /\ queue' = Append(queue, t)
    /\ UNCHANGED <<count, sent, want, outcomes, took>>

\* 資源 o を待つ待機を登録順に評価する。結果: 数、成立したスレッドと取った資源、残りの列。
RECURSIVE Settle(_, _, _)
Settle(o, q, c) ==
    IF q = <<>> THEN [count |-> c, woke |-> {}, rest |-> <<>>]
    ELSE LET t == Head(q)
             d == Decide(c, want[t])
         IN IF o \notin Targets(t) \/ d[1] = "Blocked"
            THEN LET r == Settle(o, Tail(q), c) IN [r EXCEPT !.rest = <<t>> \o r.rest]
            ELSE LET r == Settle(o, Tail(q), Counts(d))
                 IN [r EXCEPT !.woke = r.woke \cup {<<t, Taken(c, d)>>}]

Signal(o) ==
    /\ count[o] < 2 /\ sent[o] < MaxSignals
    /\ sent' = [sent EXCEPT ![o] = @ + 1]
    /\ LET r == Settle(o, queue, [count EXCEPT ![o] = @ + 1])
           woke == {p[1] : p \in r.woke}
       IN /\ count' = r.count
          /\ queue' = r.rest
          /\ phase' = [t \in Threads |-> IF t \in woke THEN "done" ELSE phase[t]]
          /\ outcomes' = [t \in Threads |-> IF t \in woke THEN outcomes[t] + 1 ELSE outcomes[t]]
          /\ took' = [t \in Threads |->
                         IF t \in woke THEN took[t] \cup (CHOOSE p \in r.woke : p[1] = t)[2] ELSE took[t]]
    /\ UNCHANGED want

Without(q, t) == SelectSeq(q, LAMBDA x : x # t)

\* 期限が来る。待機中なら時間切れで確定し、登録を外す。
Timeout(t) ==
    /\ \/ phase[t] = "waiting"
       \/ LateTimeout /\ phase[t] = "done" /\ outcomes[t] < 2
    /\ phase' = [phase EXCEPT ![t] = "done"]
    /\ queue' = Without(queue, t)
    /\ outcomes' = [outcomes EXCEPT ![t] = @ + 1]
    /\ UNCHANGED <<count, sent, want, took>>

\* 変種だけ: 「すべて」の待機が、成立した対象を一つずつ先に取る。
Grab(t, o) ==
    /\ PartialAll /\ phase[t] = "waiting" /\ want[t][2] = "All"
    /\ o \in Targets(t) \ took[t] /\ count[o] > 0
    /\ count' = [count EXCEPT ![o] = @ - 1]
    /\ took' = [took EXCEPT ![t] = @ \cup {o}]
    /\ IF took'[t] = Targets(t)
       THEN /\ phase' = [phase EXCEPT ![t] = "done"]
            /\ queue' = Without(queue, t)
            /\ outcomes' = [outcomes EXCEPT ![t] = @ + 1]
       ELSE UNCHANGED <<phase, queue, outcomes>>
    /\ UNCHANGED <<sent, want>>

Next ==
    \/ \E t \in Threads, w \in Orders \X Modes : Wait(t, w)
    \/ \E t \in Threads : Register(t) \/ Timeout(t)
    \/ \E o \in Objs : Signal(o)
    \/ \E t \in Threads, o \in Objs : Grab(t, o)

Spec == Init /\ [][Next]_vars

\* ---- 性質 ----
OneOutcome == \A t \in Threads : outcomes[t] <= 1

NoLostWake ==
    \A t \in Threads : phase[t] \in {"checked", "waiting"} => Decide(count, want[t])[1] = "Blocked"

Conserved ==
    \A o \in Objs : sent[o] = count[o] + Cardinality({t \in Threads : o \in took[t]})

AllOrNothing ==
    \A t \in Threads :
        IF want[t][2] = "All" THEN took[t] \in {{}, Targets(t)} ELSE Cardinality(took[t]) <= 1

QueueIsWaiters ==
    /\ \A i \in 1..Len(queue) : phase[queue[i]] = "waiting"
    /\ \A t \in Threads : phase[t] = "waiting" => \E i \in 1..Len(queue) : queue[i] = t
    /\ \A i, j \in 1..Len(queue) : queue[i] = queue[j] => i = j

\* 到達確認: 通知で起こされた「すべて」の待機がある (違反として検出されるのが正しい)。
ReachAllWokenBySignal ==
    ~ \E t \in Threads : phase[t] = "done" /\ want[t][2] = "All" /\ took[t] = Objs /\ queue = <<>> /\ sent["a"] = 1
=============================================================================
