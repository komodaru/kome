---------------------------- MODULE IoLifecycle ----------------------------
(* 一つの非同期 I/O 要求の寿命: 発行、取消の要求、ホストの応答、終端の確定、公開、回収。

   カーネルの遷移は IoTable (crates/kome/src/io.rs の io_step から生成) をそのまま使う。
   ここが加えるのは周りの動き: Personality (発行・取消・出力の書き込み・公開・回収)、
   ホスト (要求の受け取り、応答、重複した応答)、持ち主のプロセスの終了。
   状態と遷移の全体は、scripts/ci/tla.sh が実際の Kernel の動きと突き合わせる
   (crates/kome/src/conform.rs、変数の対応は docs/tla-conformance.md)。

   確かめる性質:
     SettlesOnce        終端は一度だけ確定する (台帳 IO-1)。
     NoWakeBeforeOutput 出力を書く前に待機を解除しない。
     NoReleaseInFlight  ホストが最初の応答を返す前に要求を回収しない。
     Finishes           ホストが応答すれば、発行した要求は必ず終端する。

   定数は、守るべき規則を一つずつ外した変種を作る。変種では反例が出なければならない
   (scripts/ci/tla.sh)。 *)
EXTENDS IoTable, Naturals

CONSTANTS
    AcceptDuplicate,   \* TRUE: 終端済みの要求への完了通知を受理する (誤り)
    ReleaseInFlight,   \* TRUE: 応答待ちの要求の回収を許す (誤り)
    WakeOnComplete,    \* TRUE: 完了通知を受けた時点で待機を解除する (誤り)
    HostDuplicates     \* TRUE: ホストは同じ応答を二度送ることがある

VARIABLES
    state,     \* カーネルの進行状態。IoStates のどれか、または "Gone" (回収済み)
    settled,   \* 終端を確定した回数
    effects,   \* 出したがホストが未処理の Effect
    host,      \* "idle" (未受領) / "working" (処理中) / "answered" (応答済み)
    msgs,      \* カーネルへ向かっている完了通知の数
    resent,    \* ホストが重複した応答を送ったか
    written,   \* 出力をゲストへ書いたか
    woken,     \* 待機を解除したか
    owner      \* 持ち主のプロセス: "alive" / "exited"

vars == <<state, settled, effects, host, msgs, resent, written, woken, owner>>

Terminal == {"Settled", "Published"}
InFlight == {"Issued", "CancelRequested"}

TypeOK ==
    /\ state \in IoStates \cup {"Gone"}
    /\ settled \in 0..3
    /\ effects \subseteq {"Submit", "Cancel"}
    /\ host \in {"idle", "working", "answered"}
    /\ msgs \in 0..2
    /\ resent \in BOOLEAN /\ written \in BOOLEAN /\ woken \in BOOLEAN
    /\ owner \in {"alive", "exited"}

Init ==
    /\ state = "Prepared" /\ settled = 0 /\ effects = {} /\ host = "idle" /\ msgs = 0
    /\ resent = FALSE /\ written = FALSE /\ woken = FALSE /\ owner = "alive"

\* カーネルが出来事 e を受理する。遷移と Effect は生成した表のとおり。
Step(e) ==
    /\ <<state, e>> \in DOMAIN IoStep
    /\ LET next == IoStep[<<state, e>>][1]
           emit == IoStep[<<state, e>>][2]
       IN /\ state' = next
          /\ effects' = IF emit = "Nothing" THEN effects ELSE effects \cup {emit}
          /\ settled' = IF state \notin Terminal /\ next \in Terminal THEN settled + 1 ELSE settled

\* ---- Personality ----
Issue  == owner = "alive" /\ Step("Issue")  /\ UNCHANGED <<host, msgs, resent, written, woken, owner>>
Cancel == owner = "alive" /\ Step("Cancel") /\ UNCHANGED <<host, msgs, resent, written, woken, owner>>

\* 確定した結果の出力を、公開の前にゲストへ書く。
WriteOutput ==
    /\ owner = "alive" /\ state = "Settled" /\ ~written
    /\ written' = TRUE
    /\ UNCHANGED <<state, settled, effects, host, msgs, resent, woken, owner>>

Publish ==
    /\ owner = "alive" /\ written /\ Step("Publish")
    /\ woken' = TRUE
    /\ UNCHANGED <<host, msgs, resent, written, owner>>

Release ==
    /\ owner = "alive"
    /\ \/ state \in {"Prepared"} \cup Terminal
       \/ ReleaseInFlight /\ state \in InFlight
    /\ state' = "Gone"
    /\ UNCHANGED <<settled, effects, host, msgs, resent, written, woken, owner>>

\* ---- ホスト ----
HostTake ==
    /\ "Submit" \in effects /\ host = "idle"
    /\ host' = "working" /\ effects' = effects \ {"Submit"}
    /\ UNCHANGED <<state, settled, msgs, resent, written, woken, owner>>

\* 取消の要求を受け取る。応答の内容 (取消か正常完了か) はここでは区別しない。
HostSeeCancel ==
    /\ "Cancel" \in effects
    /\ effects' = effects \ {"Cancel"}
    /\ UNCHANGED <<state, settled, host, msgs, resent, written, woken, owner>>

HostAnswer ==
    /\ host = "working"
    /\ host' = "answered" /\ msgs' = msgs + 1
    /\ UNCHANGED <<state, settled, effects, resent, written, woken, owner>>

HostResend ==
    /\ HostDuplicates /\ host = "answered" /\ ~resent
    /\ resent' = TRUE /\ msgs' = msgs + 1
    /\ UNCHANGED <<state, settled, effects, host, written, woken, owner>>

\* ---- 完了通知の到着 ----
Deliver ==
    /\ msgs > 0 /\ msgs' = msgs - 1
    /\ IF <<state, "Complete">> \in DOMAIN IoStep
       THEN /\ settled' = settled + 1
            /\ effects' = effects
            \* 持ち主が終了済みなら、確定と同時に回収する。受け取るゲストはいない。
            /\ state' = IF owner = "exited" THEN "Gone" ELSE IoStep[<<state, "Complete">>][1]
            /\ woken' = (woken \/ (WakeOnComplete /\ owner = "alive"))
       ELSE /\ settled' = IF AcceptDuplicate /\ state \in Terminal THEN settled + 1 ELSE settled
            /\ UNCHANGED <<state, effects, woken>>
    /\ UNCHANGED <<host, resent, written, owner>>

\* ---- プロセスの終了 ----
OwnerExit ==
    /\ owner = "alive" /\ owner' = "exited"
    /\ IF state = "Issued"
       THEN Step("Cancel")
       ELSE /\ state' = IF state = "CancelRequested" THEN state ELSE "Gone"
            /\ UNCHANGED <<settled, effects>>
    /\ UNCHANGED <<host, msgs, resent, written, woken>>

Next ==
    \/ Issue \/ Cancel \/ WriteOutput \/ Publish \/ Release
    \/ HostTake \/ HostSeeCancel \/ HostAnswer \/ HostResend
    \/ Deliver \/ OwnerExit

Spec == Init /\ [][Next]_vars /\ WF_vars(HostTake) /\ WF_vars(HostAnswer) /\ WF_vars(Deliver)

\* ---- 性質 ----
SettlesOnce == settled <= 1

NoWakeBeforeOutput == woken => written

\* ホストが最初の応答をまだ返していないのに、要求の記録が無い。
NoReleaseInFlight ==
    state = "Gone" => /\ "Submit" \notin effects
                      /\ host # "working"
                      /\ (msgs = 0 \/ settled >= 1)

Finishes == (state \in InFlight) ~> (state \in Terminal \cup {"Gone"})

\* 到達確認: 発行した要求が応答を受けて公開まで進む実行がある (違反として検出されるのが正しい)。
ReachPublished == ~(state = "Published" /\ host = "answered" /\ "Cancel" \notin effects /\ woken)
=============================================================================
