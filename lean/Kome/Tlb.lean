/-!
# 速い経路の表 (crates/kome/src/fast.rs) のモデル

表 (`SoftTlb`) は、ゲストの頁から実体の頁への覚え書きである。ここでは、アドレス空間を変える操作と
表を使う操作を、どんな順に何回行っても、表に当たるアクセスが今のアドレス空間でも許されることを示す
(台帳 MEM-4)。Kani が示すのは一つの項目の照合 (番地の計算) で、操作の列については示していない。

モデルが実装と同じ動きをすることは、`mirror` (Mirror.lean) の出力を実際の `SoftTlb` と比べて確かめる
(crates/kome-testkit/tests/lean.rs)。

- `View`: 一つの頁の見え方。`spot` は実体の頁の番号で、どの別名から見ても同じ頁なら同じ番号である。
- `State`: プロセスごとのアドレス空間と表と「書かれた頁」、全体で一つの「変換したコードの頁」。
- `step`: 一つの操作。実装が拒む操作 (重なる範囲へのマッピング) は、状態を変えない。
-/

namespace Kome.Tlb

/-- 一つの頁の見え方。 -/
structure View where
  spot : Nat
  readable : Bool
  writable : Bool
  executable : Bool
deriving DecidableEq, Repr

/-- 表の一項目。`read` と `write` は、その側が当たるか。 -/
structure Entry where
  page : Nat
  spot : Nat
  read : Bool
  write : Bool
deriving DecidableEq, Repr

/-- 全体の状態。プロセスは番号で表す。 -/
structure State where
  /-- プロセスと頁から、見え方。 -/
  space : Nat → Nat → Option View
  /-- プロセスと項目の位置から、項目。 -/
  table : Nat → Nat → Option Entry
  /-- 変換したコードの元になった実体の頁か。 -/
  code : Nat → Bool
  /-- プロセスごとの、速い経路で書けるようにした実体の頁か。 -/
  dirty : Nat → Nat → Bool

/-- 何も無い状態。 -/
def State.empty : State :=
  { space := fun _ _ => none, table := fun _ _ => none, code := fun _ => false, dirty := fun _ _ => false }

/-- 操作。`slots` (項目の数) は `step` の引数で、操作には入らない。 -/
inductive Op where
  /-- `first` から `count` 頁を、実体の頁 `spot` から順に見せる。範囲が空いていなければ拒む。 -/
  | map (p first count spot : Nat) (r w x : Bool)
  /-- `first` から `count` 頁を外す。 -/
  | unmap (p first count : Nat)
  /-- `first` から `count` 頁の権限を変える。見え方の無い頁が範囲にあれば拒む。 -/
  | protect (p first count : Nat) (r w x : Bool)
  /-- `page` の項目を、読み出し (`write = false`) か書き込みで埋める。 -/
  | fill (p page : Nat) (write : Bool)
  /-- `page` のコードを変換する。その実体の頁は、全ての表で書けなくなる。 -/
  | translate (p page : Nat)
  /-- 実体の頁 `spot` のコードを捨てる。 -/
  | release (spot : Nat)
  /-- 書かれた頁を取り出す。 -/
  | take (p : Nat)
  /-- 表を空にする。 -/
  | flush (p : Nat)
  /-- プロセスが無くなった。 -/
  | cleared (p : Nat)
deriving Repr

/-- `page` が `first` から `count` 頁の範囲にあるか。 -/
def inRange (first count page : Nat) : Bool :=
  decide (first ≤ page) && decide (page < first + count)

/-- 範囲のどの頁にも、見え方が無いか。 -/
def free (space : Nat → Option View) (first count : Nat) : Bool :=
  (List.range count).all fun i => (space (first + i)).isNone

/-- 範囲の全ての頁に、見え方があるか。 -/
def covered (space : Nat → Option View) (first count : Nat) : Bool :=
  (List.range count).all fun i => (space (first + i)).isSome

/-- 範囲の全ての頁が、もうその権限か。 -/
def settled (space : Nat → Option View) (first count : Nat) (r w x : Bool) : Bool :=
  (List.range count).all fun i =>
    match space (first + i) with
    | some v => v.readable == r && v.writable == w && v.executable == x
    | none => false

/-- 範囲に入る頁の項目を消す。 -/
def dropRange (table : Nat → Option Entry) (first count : Nat) : Nat → Option Entry :=
  fun i => match table i with
    | some e => if inRange first count e.page then none else some e
    | none => none

/-- 実体の頁 `spot` を写している項目の、書き込みの側を空ける。 -/
def barWrites (table : Nat → Option Entry) (spot : Nat) : Nat → Option Entry :=
  fun i => match table i with
    | some e => if e.spot = spot then some { e with write := false } else some e
    | none => none

/-- 全ての項目の、書き込みの側を空ける。 -/
def barAll (table : Nat → Option Entry) : Nat → Option Entry :=
  fun i => (table i).map fun e => { e with write := false }

/-- `page` の項目を埋める。許されないアクセスと、コードの頁への書き込みは、何も変えない。 -/
def fill (slots : Nat) (s : State) (p page : Nat) (write : Bool) : State :=
  match s.space p page with
  | none => s
  | some v =>
    if (if write then v.writable else v.readable) = false then s
    else if write = true ∧ s.code v.spot = true then s
    else
      { s with
        dirty := fun q spot => if q = p ∧ spot = v.spot ∧ write = true then true else s.dirty q spot
        table := fun q i =>
          if q = p ∧ i = page % slots then
            some { page := page, spot := v.spot, read := v.readable,
                   write := v.writable && !s.code v.spot && (write || s.dirty p v.spot) }
          else s.table q i }

/-- `page` のコードを変換する。実行できる頁だけが通り、その実体の頁は全ての表で書けなくなる。 -/
def translate (s : State) (p page : Nat) : State :=
  match s.space p page with
  | none => s
  | some v =>
    if v.executable = true then
      { s with
        code := fun spot => if spot = v.spot then true else s.code spot
        table := fun q => barWrites (s.table q) v.spot }
    else s

/-- `first` から `count` 頁の権限を変える。見え方の無い頁が範囲にあれば拒み、何も変えない。
範囲の全体がもうその権限なら、何も変えない (実装は通知を出さず、表の項目も残る)。 -/
def protect (s : State) (p first count : Nat) (r w x : Bool) : State :=
  if covered (s.space p) first count = true ∧ settled (s.space p) first count r w x = false then
    { s with
      space := fun q page =>
        if q = p ∧ inRange first count page = true then
          (s.space p page).map fun v => { v with readable := r, writable := w, executable := x }
        else s.space q page
      table := fun q => if q = p then dropRange (s.table p) first count else s.table q }
  else s

/-- 埋める操作の結果の種類。 -/
inductive Filled where
  /-- 項目を埋めた。 -/
  | filled
  /-- 基準の経路でも失敗するアクセス。 -/
  | fault
  /-- 速い経路に載せない (コードの頁への書き込み)。 -/
  | slow
deriving DecidableEq, Repr

/-- `fill` が何をするか。 -/
def fillVerdict (s : State) (p page : Nat) (write : Bool) : Filled :=
  match s.space p page with
  | none => .fault
  | some v =>
    if (if write then v.writable else v.readable) = false then .fault
    else if write = true ∧ s.code v.spot = true then .slow
    else .filled

/-- 一つの操作。 -/
def step (slots : Nat) (s : State) : Op → State
  | .map p first count spot r w x =>
    if free (s.space p) first count then
      { s with space := fun q page =>
          if q = p ∧ inRange first count page then
            some { spot := spot + (page - first), readable := r, writable := w, executable := x }
          else s.space q page }
    else s
  | .unmap p first count =>
    { s with
      space := fun q page => if q = p ∧ inRange first count page then none else s.space q page
      table := fun q => if q = p then dropRange (s.table p) first count else s.table q }
  | .protect p first count r w x => protect s p first count r w x
  | .fill p page write => fill slots s p page write
  | .translate p page => translate s p page
  | .release spot => { s with code := fun other => if other = spot then false else s.code other }
  | .take p =>
    { s with
      dirty := fun q spot => if q = p then false else s.dirty q spot
      table := fun q => if q = p then barAll (s.table p) else s.table q }
  | .flush p => { s with table := fun q i => if q = p then none else s.table q i }
  | .cleared p =>
    { s with
      space := fun q page => if q = p then none else s.space q page
      table := fun q i => if q = p then none else s.table q i
      dirty := fun q spot => if q = p then false else s.dirty q spot }

/-- 操作の列を、何も無い状態から順に行う。 -/
def run (slots : Nat) (ops : List Op) : State :=
  ops.foldl (step slots) State.empty

/-- 変換したコードが行う照合。当たれば、実体の頁。 -/
def probe (slots : Nat) (s : State) (p page : Nat) (write : Bool) : Option Nat :=
  match s.table p (page % slots) with
  | some e => if e.page = page ∧ (if write then e.write else e.read) = true then some e.spot else none
  | none => none

/-- 項目が、今のアドレス空間と合っている。 -/
def Entry.Agrees (s : State) (p : Nat) (e : Entry) : Prop :=
  ∃ v, s.space p e.page = some v ∧ e.spot = v.spot ∧
    (e.read = true → v.readable = true) ∧
    (e.write = true → v.writable = true ∧ s.code v.spot = false ∧ s.dirty p v.spot = true)

/-- 全ての項目が、今のアドレス空間と合っている。 -/
def Ok (s : State) : Prop :=
  ∀ p i e, s.table p i = some e → e.Agrees s p

theorem pick_then {c : Prop} [Decidable c] {α : Type} {a b : α} (h : c) : (if c then a else b) = a := by
  simp [h]

theorem pick_else {c : Prop} [Decidable c] {α : Type} {a b : α} (h : ¬c) : (if c then a else b) = b := by
  simp [h]

theorem ok_empty : Ok State.empty := by
  intro p i e h
  simp [State.empty] at h

/-- 空いている範囲の頁には、見え方が無い。 -/
theorem free_none {space : Nat → Option View} {first count page : Nat}
    (hfree : free space first count = true) (hin : inRange first count page = true) :
    space page = none := by
  simp only [inRange, Bool.and_eq_true, decide_eq_true_eq] at hin
  simp only [free, List.all_eq_true, List.mem_range] at hfree
  have h := hfree (page - first) (by omega)
  have hpage : first + (page - first) = page := by omega
  rw [hpage] at h
  simpa using h

/-- 範囲の外の項目だけが残る。 -/
theorem dropRange_some {table : Nat → Option Entry} {first count i : Nat} {e : Entry}
    (h : dropRange table first count i = some e) :
    table i = some e ∧ inRange first count e.page = false := by
  unfold dropRange at h
  cases ht : table i with
  | none => simp [ht] at h
  | some e0 =>
    simp only [ht] at h
    by_cases hin : inRange first count e0.page = true
    · simp [hin] at h
    · simp only [hin] at h
      have he : e0 = e := by simpa using h
      subst he
      exact ⟨rfl, by simpa using hin⟩

/-- 書き込みの側を空けた項目は、元の項目の書き込みの側だけが違う。 -/
theorem barWrites_some {table : Nat → Option Entry} {spot i : Nat} {e : Entry}
    (h : barWrites table spot i = some e) :
    ∃ e0, table i = some e0 ∧ e.page = e0.page ∧ e.spot = e0.spot ∧ e.read = e0.read ∧
      (e.write = true → e0.write = true ∧ e0.spot ≠ spot) := by
  unfold barWrites at h
  cases ht : table i with
  | none => simp [ht] at h
  | some e0 =>
    simp only [ht] at h
    by_cases hs : e0.spot = spot
    · rw [pick_then hs] at h
      have he : { e0 with write := false } = e := by simpa using h
      subst he
      exact ⟨e0, rfl, rfl, rfl, rfl, by simp⟩
    · rw [pick_else hs] at h
      have he : e0 = e := by simpa using h
      subst he
      exact ⟨e0, rfl, rfl, rfl, rfl, fun hw => ⟨hw, hs⟩⟩

/-- 全ての書き込みの側を空けた項目は、書き込みでは当たらない。 -/
theorem barAll_some {table : Nat → Option Entry} {i : Nat} {e : Entry}
    (h : barAll table i = some e) :
    ∃ e0, table i = some e0 ∧ e.page = e0.page ∧ e.spot = e0.spot ∧ e.read = e0.read ∧ e.write = false := by
  unfold barAll at h
  cases ht : table i with
  | none => simp [ht] at h
  | some e0 =>
    simp only [ht, Option.map_some] at h
    have he : { e0 with write := false } = e := by simpa using h
    subst he
    exact ⟨e0, rfl, rfl, rfl, rfl, rfl⟩

theorem ok_fill (slots : Nat) (s : State) (p page : Nat) (write : Bool) (h : Ok s) :
    Ok (fill slots s p page write) := by
  unfold fill
  cases hv : s.space p page with
  | none => exact h
  | some v =>
    dsimp only
    by_cases hallowed : (if write then v.writable else v.readable) = false
    · rw [pick_then hallowed]
      exact h
    · rw [pick_else hallowed]
      by_cases hcode : write = true ∧ s.code v.spot = true
      · rw [pick_then hcode]
        exact h
      · rw [pick_else hcode]
        intro q i e he
        dsimp only at he
        by_cases hslot : q = p ∧ i = page % slots
        · rw [pick_then hslot] at he
          obtain ⟨hq, _⟩ := hslot
          subst hq
          have he' := Option.some.inj he
          subst he'
          refine ⟨v, hv, rfl, fun hr => hr, ?_⟩
          intro hw
          simp only [Bool.and_eq_true, Bool.not_eq_true', Bool.or_eq_true] at hw
          refine ⟨hw.1.1, hw.1.2, ?_⟩
          show (if q = q ∧ v.spot = v.spot ∧ write = true then true else s.dirty q v.spot) = true
          cases hw.2 with
          | inl hwrite => simp [hwrite]
          | inr hdirty => simp [hdirty]
        · rw [pick_else hslot] at he
          obtain ⟨v0, hv0, hspot, hread, hwrite⟩ := h q i e he
          refine ⟨v0, hv0, hspot, hread, ?_⟩
          intro hw
          obtain ⟨h1, h2, h3⟩ := hwrite hw
          refine ⟨h1, h2, ?_⟩
          show (if q = p ∧ v0.spot = v.spot ∧ write = true then true else s.dirty q v0.spot) = true
          by_cases hset : q = p ∧ v0.spot = v.spot ∧ write = true
          · rw [pick_then hset]
          · rw [pick_else hset]
            exact h3

theorem ok_protect (s : State) (p first count : Nat) (r w x : Bool) (h : Ok s) :
    Ok (protect s p first count r w x) := by
  unfold protect
  by_cases hcovered : covered (s.space p) first count = true ∧ settled (s.space p) first count r w x = false
  · rw [pick_then hcovered]
    intro q i e he
    dsimp only at he
    by_cases hq : q = p
    · subst hq
      simp only at he
      obtain ⟨ht, hout⟩ := dropRange_some he
      obtain ⟨v, hv, rest⟩ := h q i e ht
      refine ⟨v, ?_, rest⟩
      show (if q = q ∧ inRange first count e.page = true then _ else s.space q e.page) = some v
      simp [hout, hv]
    · simp only [pick_else hq] at he
      obtain ⟨v, hv, rest⟩ := h q i e he
      refine ⟨v, ?_, rest⟩
      show (if q = p ∧ inRange first count e.page = true then _ else s.space q e.page) = some v
      simp [hq, hv]
  · rw [pick_else hcovered]
    exact h

/-- 埋めたと答えるのは、表に当たるようになるときだけである。 -/
theorem a_fill_that_reports_filled_hits (slots : Nat) (s : State) (p page : Nat) (write : Bool)
    (hslots : 0 < slots) (h : fillVerdict s p page write = .filled) :
    (probe slots (fill slots s p page write) p page write).isSome = true := by
  have _ := hslots
  unfold fillVerdict at h
  unfold fill probe
  cases hv : s.space p page with
  | none => simp [hv] at h
  | some v =>
    simp only [hv] at h
    dsimp only
    by_cases hallowed : (if write then v.writable else v.readable) = false
    · rw [pick_then hallowed] at h
      cases h
    · by_cases hcode : write = true ∧ s.code v.spot = true
      · rw [pick_else hallowed, pick_then hcode] at h
        cases h
      · rw [pick_else hallowed, pick_else hcode]
        dsimp only
        rw [pick_then ⟨rfl, rfl⟩]
        cases write with
        | false => simpa using hallowed
        | true =>
          simp only [not_and, Bool.not_eq_true, forall_const] at hcode
          simp only [↓reduceIte, Bool.not_eq_false] at hallowed
          simp [hallowed, hcode]

theorem ok_translate (s : State) (p page : Nat) (h : Ok s) : Ok (translate s p page) := by
  unfold translate
  cases hv : s.space p page with
  | none => exact h
  | some v =>
    dsimp only
    by_cases hx : v.executable = true
    · rw [pick_then hx]
      intro q i e he
      dsimp only at he
      obtain ⟨e0, ht, hpage, hspot, hread, hwrite⟩ := barWrites_some he
      obtain ⟨v0, hv0, hspot0, hread0, hwrite0⟩ := h q i e0 ht
      refine ⟨v0, by rw [hpage]; exact hv0, by rw [hspot]; exact hspot0, fun hr => hread0 (hread ▸ hr), ?_⟩
      intro hw
      obtain ⟨hw0, hne⟩ := hwrite hw
      obtain ⟨h1, h2, h3⟩ := hwrite0 hw0
      refine ⟨h1, ?_, h3⟩
      show (if v0.spot = v.spot then true else s.code v0.spot) = false
      rw [pick_else (by rw [← hspot0]; exact hne)]
      exact h2
    · rw [pick_else hx]
      exact h

/-- どの操作も、項目とアドレス空間の一致を保つ。 -/
theorem ok_step (slots : Nat) (s : State) (op : Op) (h : Ok s) : Ok (step slots s op) := by
  cases op with
  | map p first count spot r w x =>
    simp only [step]
    split
    · rename_i hfree
      intro q i e he
      obtain ⟨v, hv, rest⟩ := h q i e he
      refine ⟨v, ?_, rest⟩
      show (if q = p ∧ inRange first count e.page = true then _ else s.space q e.page) = some v
      split
      · rename_i hin
        obtain ⟨hq, hin⟩ := hin
        subst hq
        rw [free_none hfree hin] at hv
        cases hv
      · exact hv
    · exact h
  | unmap p first count =>
    intro q i e he
    simp only [step] at he
    by_cases hq : q = p
    · subst hq
      simp only at he
      obtain ⟨ht, hout⟩ := dropRange_some he
      obtain ⟨v, hv, rest⟩ := h q i e ht
      refine ⟨v, ?_, rest⟩
      show (if q = q ∧ inRange first count e.page = true then none else s.space q e.page) = some v
      simp [hout, hv]
    · simp only [pick_else hq] at he
      obtain ⟨v, hv, rest⟩ := h q i e he
      refine ⟨v, ?_, rest⟩
      show (if q = p ∧ inRange first count e.page = true then none else s.space q e.page) = some v
      simp [hq, hv]
  | protect p first count r w x => exact ok_protect s p first count r w x h
  | fill p page write => exact ok_fill slots s p page write h
  | translate p page => exact ok_translate s p page h
  | release spot =>
    intro q i e he
    obtain ⟨v, hv, hspot, hread, hwrite⟩ := h q i e he
    refine ⟨v, hv, hspot, hread, ?_⟩
    intro hw
    obtain ⟨h1, h2, h3⟩ := hwrite hw
    refine ⟨h1, ?_, h3⟩
    show (if v.spot = spot then false else s.code v.spot) = false
    split
    · rfl
    · exact h2
  | take p =>
    intro q i e he
    simp only [step] at he
    by_cases hq : q = p
    · subst hq
      simp only at he
      obtain ⟨e0, ht, hpage, hspot, hread, hwrite⟩ := barAll_some he
      obtain ⟨v0, hv0, hspot0, hread0, _⟩ := h q i e0 ht
      refine ⟨v0, by rw [hpage]; exact hv0, by rw [hspot]; exact hspot0, fun hr => hread0 (hread ▸ hr), ?_⟩
      intro hw
      rw [hwrite] at hw
      cases hw
    · simp only [pick_else hq] at he
      obtain ⟨v, hv, hspot, hread, hwrite⟩ := h q i e he
      refine ⟨v, hv, hspot, hread, ?_⟩
      intro hw
      obtain ⟨h1, h2, h3⟩ := hwrite hw
      refine ⟨h1, h2, ?_⟩
      show (if q = p then false else s.dirty q v.spot) = true
      rw [pick_else hq]
      exact h3
  | flush p =>
    intro q i e he
    simp only [step] at he
    by_cases hq : q = p
    · simp [hq] at he
    · simp only [pick_else hq] at he
      exact h q i e he
  | cleared p =>
    intro q i e he
    simp only [step] at he
    by_cases hq : q = p
    · simp [hq] at he
    · simp only [pick_else hq] at he
      obtain ⟨v, hv, hspot, hread, hwrite⟩ := h q i e he
      refine ⟨v, ?_, hspot, hread, ?_⟩
      · show (if q = p then none else s.space q e.page) = some v
        rw [pick_else hq]
        exact hv
      · intro hw
        obtain ⟨h1, h2, h3⟩ := hwrite hw
        refine ⟨h1, h2, ?_⟩
        show (if q = p then false else s.dirty q v.spot) = true
        rw [pick_else hq]
        exact h3

/-- 操作をどんな順に何回行っても、全ての項目は今のアドレス空間と合っている。 -/
theorem ok_run (slots : Nat) (ops : List Op) : Ok (run slots ops) := by
  have from_ok : ∀ (ops : List Op) (s : State), Ok s → Ok (ops.foldl (step slots) s) := by
    intro ops
    induction ops with
    | nil => intro s h; exact h
    | cons op rest ih => intro s h; exact ih (step slots s op) (ok_step slots s op h)
  exact from_ok ops State.empty ok_empty

/-- 表に当たるアクセスは、操作をどんな順に何回行った後でも、今のアドレス空間で許されている。
読み出しで当たれば、その頁は読める。書き込みで当たれば、その頁は書けて、変換したコードの頁ではなく、
書かれた頁に入っている。当たった実体の頁は、今その頁が見せているものである。 -/
theorem a_hit_is_allowed_after_any_operations (slots : Nat) (ops : List Op) (p page : Nat) (write : Bool)
    (spot : Nat) (hit : probe slots (run slots ops) p page write = some spot) :
    ∃ v, (run slots ops).space p page = some v ∧ v.spot = spot ∧
      (write = false → v.readable = true) ∧
      (write = true → v.writable = true ∧ (run slots ops).code spot = false ∧
        (run slots ops).dirty p spot = true) := by
  unfold probe at hit
  cases ht : (run slots ops).table p (page % slots) with
  | none => simp [ht] at hit
  | some e =>
    simp only [ht] at hit
    by_cases hmatch : e.page = page ∧ (if write = true then e.write else e.read) = true
    · rw [pick_then hmatch] at hit
      obtain ⟨hpage, hside⟩ := hmatch
      have hspot : e.spot = spot := Option.some.inj hit
      obtain ⟨v, hv, hes, hread, hwrite⟩ := ok_run slots ops p (page % slots) e ht
      refine ⟨v, by rw [← hpage]; exact hv, by rw [← hes]; exact hspot, ?_, ?_⟩
      · intro hw
        rw [hw] at hside
        exact hread hside
      · intro hw
        rw [hw] at hside
        obtain ⟨h1, h2, h3⟩ := hwrite hside
        rw [← hspot, hes]
        exact ⟨h1, h2, h3⟩
    · rw [pick_else hmatch] at hit
      cases hit

end Kome.Tlb
