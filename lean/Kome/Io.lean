import Kome.IoTable

/-!
I/O 要求の一生を、出来事の列について証明する (台帳 IO-1)。

遷移の表 `ioStep` は crates/kome/src/io.rs の io_step から生成したものである
(lean/Kome/IoTable.lean)。ここでは、出来事をどんな順に何個与えても成り立つことを示す。
拒否された出来事は状態を変えず、ホストへ何も出さない。
-/

namespace Kome.Io

open Kome.IoTable

/-- 出来事を一つ与える。表に無い組は拒否で、状態はそのまま、出すものは無い。 -/
def apply (s : IoState) (e : IoEvent) : IoState × IoEmit :=
  match ioStep s e with
  | some r => r
  | none => (s, .Nothing)

/-- 出来事を一つ与えた後の状態。 -/
def next (s : IoState) (e : IoEvent) : IoState := (apply s e).1

/-- 出来事を一つ与えたときにホストへ出すもの。 -/
def emit (s : IoState) (e : IoEvent) : IoEmit := (apply s e).2

/-- 出来事の列を与えた後の状態。 -/
def run : IoState → List IoEvent → IoState
  | s, [] => s
  | s, e :: es => run (next s e) es

/-- 出来事の列を与える間に、`tick` が数えた回数の合計。 -/
def count (tick : IoState → IoEvent → Nat) : IoState → List IoEvent → Nat
  | _, [] => 0
  | s, e :: es => tick s e + count tick (next s e) es

/-- 一歩ごとに「数えた分と残りの枠の和が、前の枠を超えない」なら、列の全体でも枠を超えない。 -/
theorem count_le (tick : IoState → IoEvent → Nat) (budget : IoState → Nat)
    (h : ∀ s e, tick s e + budget (next s e) ≤ budget s) :
    ∀ (es : List IoEvent) (s : IoState), count tick s es ≤ budget s
  | [], _ => Nat.zero_le _
  | e :: es, s =>
    Nat.le_trans (Nat.add_le_add_left (count_le tick budget h es (next s e)) (tick s e)) (h s e)

/-- 終端に入った一歩 (終端していない状態から Settled へ)。 -/
def settles (s : IoState) (e : IoEvent) : Nat :=
  if next s e = .Settled ∧ s ≠ .Settled then 1 else 0

/-- ホストへ Submit を出した一歩。 -/
def submits (s : IoState) (e : IoEvent) : Nat :=
  if emit s e = .Submit then 1 else 0

/-- ホストへ Cancel を出した一歩。 -/
def cancels (s : IoState) (e : IoEvent) : Nat :=
  if emit s e = .Cancel then 1 else 0

/-- ホストへ何かを出した一歩。 -/
def emits (s : IoState) (e : IoEvent) : Nat :=
  if emit s e = .Nothing then 0 else 1

/-- 状態の進み具合。 -/
def rank : IoState → Nat
  | .Prepared => 0
  | .Issued => 1
  | .CancelRequested => 2
  | .Settled => 3
  | .Published => 4

/-- まだ終端できる回数。 -/
def settleBudget (s : IoState) : Nat := if rank s < 3 then 1 else 0

/-- まだ Submit を出せる回数。 -/
def submitBudget (s : IoState) : Nat := if rank s < 1 then 1 else 0

/-- まだ Cancel を出せる回数。 -/
def cancelBudget (s : IoState) : Nat := if rank s < 2 then 1 else 0

/-- まだホストへ何かを出せる回数。 -/
def emitBudget (s : IoState) : Nat := 2 - rank s

/-- 出来事をどんな順に何個与えても、終端に入るのは多くて一度である。 -/
theorem an_io_settles_at_most_once (s : IoState) (es : List IoEvent) : count settles s es ≤ 1 :=
  Nat.le_trans
    (count_le settles settleBudget (by intro s e; cases s <;> cases e <;> decide) es s)
    (by cases s <;> decide)

/-- 終端した後は、どんな出来事の列でも二度と終端に入らない。 -/
theorem a_settled_io_never_settles_again (es : List IoEvent) : count settles .Settled es = 0 :=
  Nat.le_zero.mp
    (count_le settles settleBudget (by intro s e; cases s <;> cases e <;> decide) es .Settled)

/-- 出来事をどんな順に何個与えても、ホストへ Submit を出すのは多くて一度である。 -/
theorem an_io_is_submitted_at_most_once (s : IoState) (es : List IoEvent) : count submits s es ≤ 1 :=
  Nat.le_trans
    (count_le submits submitBudget (by intro s e; cases s <;> cases e <;> decide) es s)
    (by cases s <;> decide)

/-- 出来事をどんな順に何個与えても、ホストへ Cancel を出すのは多くて一度である。 -/
theorem a_cancel_reaches_the_host_at_most_once (s : IoState) (es : List IoEvent) :
    count cancels s es ≤ 1 :=
  Nat.le_trans
    (count_le cancels cancelBudget (by intro s e; cases s <;> cases e <;> decide) es s)
    (by cases s <;> decide)

/-- 終端した後は、どんな出来事の列でもホストへ何も出さない。 -/
theorem nothing_goes_to_the_host_after_settling (es : List IoEvent) : count emits .Settled es = 0 :=
  Nat.le_zero.mp
    (count_le emits emitBudget (by intro s e; cases s <;> cases e <;> decide) es .Settled)

/-- 状態は後ろへ戻らない (一歩)。 -/
theorem a_step_never_goes_back (s : IoState) (e : IoEvent) : rank s ≤ rank (next s e) := by
  cases s <;> cases e <;> decide

/-- 出来事をどんな順に何個与えても、状態は後ろへ戻らない。終端した要求は終端したままである。 -/
theorem an_io_never_goes_back : ∀ (es : List IoEvent) (s : IoState), rank s ≤ rank (run s es)
  | [], _ => Nat.le_refl _
  | e :: es, s => Nat.le_trans (a_step_never_goes_back s e) (an_io_never_goes_back es (next s e))

/-- 公開した要求は、どの出来事も拒否する。 -/
theorem a_published_io_refuses_everything (e : IoEvent) : ioStep .Published e = none := by
  cases e <;> rfl

end Kome.Io
