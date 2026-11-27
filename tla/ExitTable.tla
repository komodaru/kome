----------------------------- MODULE ExitTable -----------------------------
(* crates/kome の Kernel を実際に動かして生成した。手で書き換えない。
   書き直す: KOME_UPDATE_GOLDEN=1 cargo test -p kome the_tla_exit_table_is_the_kernel *)
EXTENDS TLC

\* (終了の要求, スレッドの状態) から (要求の直後の状態, 中断を要求したか) への表。
\* 実行中のスレッドは、実行権を返したときに終了する。
ExitThread ==
    (<<"ExitProcess", "Ready">> :> <<"Gone", FALSE>>) @@
    (<<"ExitProcess", "Running">> :> <<"Running", TRUE>>) @@
    (<<"ExitProcess", "Servicing">> :> <<"Gone", FALSE>>) @@
    (<<"ExitProcess", "Blocked">> :> <<"Gone", FALSE>>) @@
    (<<"Terminate", "Ready">> :> <<"Gone", FALSE>>) @@
    (<<"Terminate", "Running">> :> <<"Running", TRUE>>) @@
    (<<"Terminate", "Servicing">> :> <<"Gone", FALSE>>) @@
    (<<"Terminate", "Blocked">> :> <<"Gone", FALSE>>)

\* 最後のスレッドが止まったとき: I/O の状態から (プロセスの段階, I/O の状態, 取消を要求したか) への表。
\* プロセスが持っていた参照は、このとき全て手放す。終了の通知はこのとき一度だけ出す。
ExitFinish ==
    ("None" :> <<"Reclaimed", "None", FALSE>>) @@
    ("Prepared" :> <<"Reclaimed", "Gone", FALSE>>) @@
    ("Issued" :> <<"Draining", "CancelRequested", TRUE>>) @@
    ("CancelRequested" :> <<"Draining", "CancelRequested", FALSE>>) @@
    ("Settled" :> <<"Reclaimed", "Gone", FALSE>>) @@
    ("Published" :> <<"Reclaimed", "Gone", FALSE>>)

\* 終了の通知の後に完了通知が届いたとき: I/O の状態から (プロセスの段階, I/O の状態) への表。
ExitComplete ==
    ("CancelRequested" :> <<"Reclaimed", "Gone">>)
=============================================================================
