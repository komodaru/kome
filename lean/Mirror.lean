import Kome

/-!
# モデルを実行して、実装と比べるための出力を作る

`mirror <操作の列のファイル>` は、操作を一行ずつ読んで `Kome.Tlb.step` に適用し、操作のたびに
「操作の結果の種類」と「全てのプロセスの全ての頁が、読み出しと書き込みで表に当たるか」を一行で出す。
crates/kome-testkit/tests/lean.rs が、同じ操作を実際の `Kernel` と `SoftTlb` に行い、同じ形の行を
作って比べる。行の形は、そのファイルの `Line` の説明にある。
-/

open Kome.Tlb

/-- 比べる世界の形: 項目の数、プロセスの数、頁の数、実体の頁の数。 -/
structure Shape where
  slots : Nat
  procs : Nat
  pages : Nat
  spots : Nat

def flag (word : String) : Option Bool :=
  if word = "1" then some true else if word = "0" then some false else none

def parseOp : List String → Option Op
  | ["map", p, first, count, spot, r, w, x] => do
    pure (.map (← p.toNat?) (← first.toNat?) (← count.toNat?) (← spot.toNat?) (← flag r) (← flag w) (← flag x))
  | ["unmap", p, first, count] => do pure (.unmap (← p.toNat?) (← first.toNat?) (← count.toNat?))
  | ["protect", p, first, count, r, w, x] => do
    pure (.protect (← p.toNat?) (← first.toNat?) (← count.toNat?) (← flag r) (← flag w) (← flag x))
  | ["fill", p, page, write] => do pure (.fill (← p.toNat?) (← page.toNat?) (← flag write))
  | ["translate", p, page] => do pure (.translate (← p.toNat?) (← page.toNat?))
  | ["release", spot] => do pure (.release (← spot.toNat?))
  | ["take", p] => do pure (.take (← p.toNat?))
  | ["flush", p] => do pure (.flush (← p.toNat?))
  | ["cleared", p] => do pure (.cleared (← p.toNat?))
  | _ => none

def cell : Option Nat → String
  | some spot => toString spot
  | none => "-"

/-- 全てのプロセスの全ての頁の、読み出しと書き込みの当たり方。 -/
def hits (shape : Shape) (s : State) : String :=
  " | ".intercalate ((List.range shape.procs).map fun p =>
    " ".intercalate ((List.range shape.pages).map fun page =>
      cell (probe shape.slots s p page false) ++ ":" ++ cell (probe shape.slots s p page true)))

/-- 操作の結果の種類。実装が拒む操作と、失敗を返す操作を見分ける。 -/
def verdict (shape : Shape) (s : State) : Op → String
  | .map p first count _ _ _ _ => if free (s.space p) first count then "mapped" else "overlap"
  | .protect p first count _ _ _ => if covered (s.space p) first count then "protected" else "uncovered"
  | .fill p page write =>
    match fillVerdict s p page write with
    | .filled => "filled"
    | .fault => "fault"
    | .slow => "slow"
  | .translate p page =>
    match s.space p page with
    | some v => if v.executable then "translated" else "fault"
    | none => "fault"
  | .take p =>
    "dirty " ++ " ".intercalate (((List.range shape.spots).filter fun spot => s.dirty p spot).map toString)
  | .unmap .. | .release .. | .flush .. | .cleared .. => "done"

def main (args : List String) : IO UInt32 := do
  let [path] := args | do
    IO.eprintln "使い方: mirror <操作の列のファイル>"
    return 2
  let lines ← IO.FS.lines path
  let mut shape : Shape := { slots := 1, procs := 0, pages := 0, spots := 0 }
  let mut state := State.empty
  for line in lines do
    let words := (line.splitOn " ").filter (· ≠ "")
    match words with
    | [] => pure ()
    | ["world", slots, procs, pages, spots] =>
      -- 新しい列の始まり。状態を空に戻す。
      match slots.toNat?, procs.toNat?, pages.toNat?, spots.toNat? with
      | some slots, some procs, some pages, some spots =>
        shape := { slots := slots, procs := procs, pages := pages, spots := spots }
        state := State.empty
        IO.println "world"
      | _, _, _, _ =>
        IO.eprintln s!"読めない行: {line}"
        return 1
    | _ =>
      match parseOp words with
      | some op =>
        let told := verdict shape state op
        state := step shape.slots state op
        IO.println s!"{told} ; {hits shape state}"
      | none =>
        IO.eprintln s!"読めない行: {line}"
        return 1
  return 0
