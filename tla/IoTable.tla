------------------------------ MODULE IoTable ------------------------------
(* crates/kome/src/io.rs の io_step から生成した。手で書き換えない。
   書き直す: KOME_UPDATE_GOLDEN=1 cargo test -p kome the_tla_table_is_io_step *)
EXTENDS TLC

IoStates == {"Prepared", "Issued", "CancelRequested", "Settled", "Published"}
IoEvents == {"Issue", "Cancel", "Complete", "Publish"}

\* 受理する (状態, 出来事) から (次の状態, 出す Effect) への表。載っていない組は拒否する。
IoStep ==
    (<<"Prepared", "Issue">> :> <<"Issued", "Submit">>) @@
    (<<"Prepared", "Cancel">> :> <<"Settled", "Nothing">>) @@
    (<<"Issued", "Cancel">> :> <<"CancelRequested", "Cancel">>) @@
    (<<"Issued", "Complete">> :> <<"Settled", "Nothing">>) @@
    (<<"CancelRequested", "Cancel">> :> <<"CancelRequested", "Nothing">>) @@
    (<<"CancelRequested", "Complete">> :> <<"Settled", "Nothing">>) @@
    (<<"Settled", "Publish">> :> <<"Published", "Nothing">>)
=============================================================================
