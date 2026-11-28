/-!
crates/kome/src/io.rs の io_step から生成した。手で書き換えない。
書き直す: KOME_UPDATE_GOLDEN=1 cargo test -p kome the_lean_table_is_io_step
-/

namespace Kome.IoTable

/-- I/O 要求の状態 (結果の値は落としてある)。 -/
inductive IoState where
  | Prepared
  | Issued
  | CancelRequested
  | Settled
  | Published
deriving DecidableEq, Repr

/-- 出来事。 -/
inductive IoEvent where
  | Issue
  | Cancel
  | Complete
  | Publish
deriving DecidableEq, Repr

/-- 遷移がホストへ出すもの。 -/
inductive IoEmit where
  | Nothing
  | Submit
  | Cancel
deriving DecidableEq, Repr

/-- 受理する (状態, 出来事) から (次の状態, 出すもの) への表。載っていない組は拒否する。 -/
def ioStep : IoState → IoEvent → Option (IoState × IoEmit)
  | .Prepared, .Issue => some (.Issued, .Submit)
  | .Prepared, .Cancel => some (.Settled, .Nothing)
  | .Issued, .Cancel => some (.CancelRequested, .Cancel)
  | .Issued, .Complete => some (.Settled, .Nothing)
  | .CancelRequested, .Cancel => some (.CancelRequested, .Nothing)
  | .CancelRequested, .Complete => some (.Settled, .Nothing)
  | .Settled, .Publish => some (.Published, .Nothing)
  | _, _ => none

end Kome.IoTable
