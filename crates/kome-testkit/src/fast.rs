//! 速い経路 ([`kome::SoftTlb`]、[`kome::LinearLayout`]、[`kome::CodePages`]) を、独立したモデルと比べる。
//!
//! [`crate::sim`] が、ゲストのアドレスでの読み書きのたびに同じバイトを速い経路でもたどり、ここへ渡す。
//! ここが持つモデルは、kome の表とは別の表現である。
//!
//! - 置いた実体の位置と長さ (`placed`)。空きを先頭から探す規則を、ここでもう一度書いている。
//! - 変換したコードの元になった頁 (`translated`)。
//! - プロセスごとの、速い経路で書けるようにした頁 (`writable`) と、実際に書いた頁 (`wrote`)。
//!
//! 確かめること (操作のたび、全てのプロセスの全ての頁について):
//!
//! - 表に当たったアドレスは、頁のモデル ([`crate::pages`]) でもそのアクセスが届き、位置は
//!   「実体を置いた位置 + 実体の中の位置」である (古い項目が残っていない)。
//! - 書き込みで当たる頁は、変換したコードの頁ではなく、次に取り出す「書かれた頁」に入っている。
//! - 埋められなかった理由は、モデルが予想するものと同じである (同じ種類の失敗、置いていない実体、
//!   コードの頁への書き込み)。

use std::collections::{BTreeMap, BTreeSet};

use kome::{
    Access, BackingId, BackingPage, CodePages, GuestAddr, Kernel, LinearLayout, MapChange, MapGen, MemFaultKind, NotFast, NotPlaced, PageSize, ProcessId,
    SoftTlb, SpaceObserver,
};

use crate::pages::{Expect, Pages, Touch};

/// 線形領域の大きさ (頁の数)。実体の合計の上限より小さくして、置けない実体が出るようにする。
const ROOM: u64 = 10;
/// 表の項目の数。比べる頁の数より少なくして、同じ項目を取り合う頁が出るようにする。
const SLOTS: usize = 8;

/// 実体の中の一つの頁 (実体と、頁の先頭の位置)。
type Spot = (BackingId, u64);

/// 速い経路でたどった一バイトの結果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reached {
    /// 表に当たった。位置から逆に引いた、実体とその中の位置。
    At(BackingId, u64),
    /// 基準の経路と同じ失敗。
    Fault,
    /// 速い経路に載せない: 線形領域に置けなかった実体。
    Slow,
    /// 速い経路に載せない: 変換したコードの頁への書き込み。
    Barred,
}

/// 速い経路の部品と、そのモデル。
#[derive(Debug)]
pub struct Fast {
    page: u64,
    layout: LinearLayout,
    tlbs: BTreeMap<ProcessId, SoftTlb<u32>>,
    code: CodePages,
    placed: BTreeMap<BackingId, (u64, u64)>,
    translated: BTreeMap<Spot, BackingPage>,
    writable: BTreeMap<ProcessId, BTreeSet<Spot>>,
    wrote: BTreeMap<ProcessId, BTreeSet<Spot>>,
}

fn told(kind: MemFaultKind) -> Expect {
    match kind {
        MemFaultKind::Unmapped => Expect::Unmapped,
        MemFaultKind::Reserved => Expect::Reserved,
        MemFaultKind::Denied => Expect::Denied,
    }
}

impl Fast {
    /// 頁の大きさ `page` (2 の冪) で作る。
    pub fn new(page: u64) -> Result<Fast, String> {
        let align = PageSize::new(page).ok_or("頁の大きさ")?;
        Ok(Fast {
            page,
            layout: LinearLayout::new(ROOM.saturating_mul(page), align),
            tlbs: BTreeMap::new(),
            code: CodePages::new(),
            placed: BTreeMap::new(),
            translated: BTreeMap::new(),
            writable: BTreeMap::new(),
            wrote: BTreeMap::new(),
        })
    }

    fn spot(&self, backing: BackingId, offset: u64) -> Spot {
        (backing, offset / self.page * self.page)
    }

    /// プロセスを作った。その表を作る。
    pub fn process_created(&mut self, process: ProcessId) -> Result<(), String> {
        let page = PageSize::new(self.page).ok_or("頁の大きさ")?;
        let tlb = SoftTlb::new(process, page, SLOTS).map_err(|error| format!("表を作れない: {error:?}"))?;
        self.tlbs.insert(process, tlb);
        Ok(())
    }

    /// モデルの規則で、`room` バイトが入る最初の空きを探す。
    fn first_gap(&self, room: u64) -> Option<u64> {
        let mut taken: Vec<(u64, u64)> = self.placed.values().copied().collect();
        taken.sort_unstable();
        let mut from = 0u64;
        for (start, len) in taken {
            if start.saturating_sub(from) >= room {
                return Some(from);
            }
            from = start + len;
        }
        ((ROOM * self.page).saturating_sub(from) >= room).then_some(from)
    }

    /// 実体を作った。線形領域へ置き、置いた位置 (か、置けなかったこと) をモデルの規則と比べる。
    /// 置けたら `true`。
    pub fn backing_created(&mut self, backing: BackingId, len: u64) -> Result<bool, String> {
        let expected = self.first_gap(len);
        let got = self.layout.place(backing, len);
        match (got, expected) {
            (Ok(at), Some(start)) if at.get() == start => {
                self.placed.insert(backing, (start, len));
                if self.layout.locate(backing) != Some((at, len)) || self.layout.len() != self.placed.len() {
                    return Err(format!("MEM-4: 置いた {backing:?} を台帳から引けない"));
                }
                Ok(true)
            }
            (Err(NotPlaced::Full), None) => Ok(false),
            (got, expected) => Err(format!("MEM-4: {backing:?} ({len} バイト) を置いた結果が {got:?}、モデルは {expected:?}")),
        }
    }

    /// 実体が解放された。台帳とコードの頁から外す。
    pub fn backing_reclaimed(&mut self, backing: BackingId) -> Result<(), String> {
        let was = self.placed.remove(&backing).map(|(start, _)| start);
        if self.layout.release(backing).map(|at| at.get()) != was {
            return Err(format!("MEM-4: {backing:?} を外した位置が、モデルの {was:?} と違う"));
        }
        self.code.forget(backing);
        self.translated.retain(|(mine, _), _| *mine != backing);
        if self.code.len() != self.translated.len() {
            return Err(format!("MEM-4: {backing:?} を外した後のコードの頁の数が、モデルと違う"));
        }
        Ok(())
    }

    /// 写しから復元した。全ての表を空にする。
    pub fn restored(&mut self) {
        for tlb in self.tlbs.values_mut() {
            tlb.flush();
        }
    }

    /// `addr` の一バイトを速い経路でたどる。表に当たらなければ埋める。`expected` は頁のモデルの予想。
    pub fn access(&mut self, kernel: &Kernel, process: ProcessId, addr: u64, writing: bool, expected: Expect) -> Result<Reached, String> {
        let access = if writing { Access::Write } else { Access::Read };
        let guest = GuestAddr::new(addr);
        let Some(tlb) = self.tlbs.get_mut(&process) else { return Err(format!("MEM-4: {process:?} の表が無い")) };
        if tlb.probe(guest, access).is_none() {
            match (tlb.fill(kernel, &self.layout, &self.code, guest, access), expected) {
                (Ok(entry), Expect::Reaches(backing, offset)) => {
                    if tlb.probe(guest, access).is_none() || !tlb.entries().contains(&entry) {
                        return Err(format!("MEM-4: {addr:#x} を {access:?} で埋めた直後に当たらない"));
                    }
                    if writing {
                        let spot = (backing, offset / self.page * self.page);
                        self.writable.entry(process).or_default().insert(spot);
                    }
                }
                (Err(NotFast::Fault(fault)), expected) if fault.addr() == guest && told(fault.kind()) == expected => return Ok(Reached::Fault),
                (Err(NotFast::Slow), Expect::Reaches(backing, offset)) => {
                    let code = writing && self.translated.contains_key(&(backing, offset / self.page * self.page));
                    return if !self.placed.contains_key(&backing) {
                        Ok(Reached::Slow)
                    } else if code {
                        Ok(Reached::Barred)
                    } else {
                        Err(format!("MEM-4: 速い経路に載るはずの {addr:#x} ({access:?}) を載せなかった"))
                    };
                }
                (got, expected) => return Err(format!("MEM-4: {addr:#x} を {access:?} で埋めた結果が {got:?}、モデルは {expected:?}")),
            }
        }
        let (backing, offset) = self.hit(process, addr, writing, expected)?.ok_or("MEM-4: 埋めた項目が消えた")?;
        if writing {
            let spot = self.spot(backing, offset);
            self.wrote.entry(process).or_default().insert(spot);
        }
        Ok(Reached::At(backing, offset))
    }

    /// `addr` が表に当たるなら、モデルと比べ、位置から逆に引いた実体とその中の位置を返す。
    fn hit(&self, process: ProcessId, addr: u64, writing: bool, expected: Expect) -> Result<Option<(BackingId, u64)>, String> {
        let access = if writing { Access::Write } else { Access::Read };
        let Some(position) = self.tlbs.get(&process).and_then(|tlb| tlb.probe(GuestAddr::new(addr), access)) else { return Ok(None) };
        let position = position.get();
        let Expect::Reaches(backing, offset) = expected else {
            return Err(format!("MEM-4: {process:?} の {addr:#x} が {access:?} で表に当たるが、モデルは {expected:?}"));
        };
        // 位置から、置いてある実体を逆に引く。
        let found = self.placed.iter().find(|(_, (start, len))| *start <= position && position - *start < *len);
        let found = found.map(|(backing, (start, _))| (*backing, position - *start));
        if found != Some((backing, offset)) {
            return Err(format!("MEM-4: {process:?} の {addr:#x} の位置 {position:#x} は {found:?}、モデルは {backing:?} の {offset:#x}"));
        }
        if writing {
            let spot = self.spot(backing, offset);
            if self.translated.contains_key(&spot) {
                return Err(format!("MEM-4: 変換したコードの頁 {spot:?} へ、{process:?} の {addr:#x} から速い経路で書ける"));
            }
            if !self.writable.get(&process).is_some_and(|pages| pages.contains(&spot)) {
                return Err(format!("MEM-4: {process:?} の {addr:#x} へ速い経路で書けるが、頁 {spot:?} は書かれた頁に入っていない"));
            }
        }
        Ok(found)
    }

    /// 全てのプロセスの、`pages` 頁ぶんの全ての頁について、表に当たるものをモデルと比べる。
    pub fn check(&self, spaces: &BTreeMap<ProcessId, Pages>, pages: u64) -> Result<(), String> {
        let within = self.page - 1;
        for process in self.tlbs.keys() {
            for page in 0..pages {
                for (writing, touch) in [(false, Touch::Read), (true, Touch::Write)] {
                    let expected = spaces.get(process).map_or(Expect::Unmapped, |model| model.expect(page, within, touch));
                    self.hit(*process, page * self.page + within, writing, expected)?;
                }
            }
        }
        Ok(())
    }

    /// `addr` のコードを変換する。モデルが実行を許す場所だけが通り、その頁は全ての表で書けなくなる。
    /// 変換できたら `true`。
    pub fn translate_code(&mut self, kernel: &Kernel, process: ProcessId, addr: u64, expected: Expect) -> Result<bool, String> {
        let guest = GuestAddr::new(addr);
        let Some(tlb) = self.tlbs.get_mut(&process) else { return Err(format!("MEM-4: {process:?} の表が無い")) };
        match (tlb.protect_code(kernel, &mut self.code, guest), expected) {
            (Ok(page), Expect::Reaches(backing, offset)) if (page.backing(), page.offset().get()) == (backing, offset / self.page * self.page) => {
                self.translated.insert((backing, page.offset().get()), page);
                for other in self.tlbs.values_mut() {
                    other.bar_writes(page);
                }
                if !self.code.holds(page) || self.code.len() != self.translated.len() {
                    return Err(format!("MEM-4: 変換した {page:?} がコードの頁に入っていない"));
                }
                Ok(true)
            }
            (Err(NotFast::Fault(fault)), expected) if fault.addr() == guest && told(fault.kind()) == expected => Ok(false),
            (got, expected) => Err(format!("MEM-4: {addr:#x} のコードの変換が {got:?}、モデルは {expected:?}")),
        }
    }

    /// 変換したコードの頁のうち `nth` 番目のコードを捨て、また速い経路で書けるようにする。
    /// 捨てたら `true`。
    pub fn release_code(&mut self, nth: u64) -> Result<bool, String> {
        let nth = usize::try_from(nth).map_err(|error| error.to_string())?;
        let Some(spot) = self.translated.keys().copied().nth(nth % self.translated.len().max(1)) else { return Ok(false) };
        let page = self.translated.remove(&spot).ok_or("モデルの頁")?;
        if !self.code.release(page) || self.code.release(page) || self.code.holds(page) || self.code.len() != self.translated.len() {
            return Err(format!("MEM-4: コードの頁 {spot:?} を一度だけ外せなかった"));
        }
        Ok(true)
    }

    /// `process` の「書かれた頁」を取り出す。速い経路で書けるようにした頁と同じでなければならず、
    /// 実際に書いた頁を全て含む。取り出した数を返す。
    pub fn take_dirty(&mut self, process: ProcessId) -> Result<usize, String> {
        let Some(tlb) = self.tlbs.get_mut(&process) else { return Err(format!("MEM-4: {process:?} の表が無い")) };
        let mut seen: BTreeSet<Spot> = BTreeSet::new();
        let mut count = 0usize;
        tlb.take_dirty(|page: BackingPage| {
            seen.insert((page.backing(), page.offset().get()));
            count += 1;
        });
        let writable = self.writable.remove(&process).unwrap_or_default();
        let wrote = self.wrote.remove(&process).unwrap_or_default();
        if count != seen.len() || seen != writable || !wrote.is_subset(&seen) {
            return Err(format!("MEM-4: {process:?} の書かれた頁が {seen:?}、書けるようにした頁は {writable:?}、書いた頁は {wrote:?}"));
        }
        Ok(count)
    }
}

impl SpaceObserver for Fast {
    fn changed(&mut self, process: ProcessId, generation: MapGen, change: MapChange) {
        // 変更は全ての表へ知らせる。自分のプロセスのものかは、表が見分ける。
        for tlb in self.tlbs.values_mut() {
            tlb.changed(process, generation, change);
        }
    }

    fn cleared(&mut self, process: ProcessId) {
        for tlb in self.tlbs.values_mut() {
            tlb.cleared(process);
        }
        self.writable.remove(&process);
        self.wrote.remove(&process);
    }
}

#[cfg(test)]
mod tests {
    use kome::{AddrWidth, BackingOffset, GuestRange, InstanceId, Limits, MapSource, Prot, map_observed};

    use super::*;
    use crate::pages::Perm;

    const PAGE: u64 = 0x1000;

    struct World {
        kernel: Kernel,
        fast: Fast,
        spaces: BTreeMap<ProcessId, Pages>,
        backing: BackingId,
    }

    impl World {
        /// プロセスを作り、実体の 2 頁を 0x1000 から、全ての権限で見せる。
        fn show(&mut self) -> ProcessId {
            let (process, _) = self.kernel.create_process(AddrWidth::Bits32, PageSize::new(PAGE).unwrap()).unwrap();
            self.fast.process_created(process).unwrap();
            if self.spaces.is_empty() {
                self.backing = self.kernel.create_backing(process, 2 * PAGE).unwrap();
                assert_eq!(self.fast.backing_created(self.backing, 2 * PAGE), Ok(true));
            } else {
                self.kernel.retain_backing(process, self.backing).unwrap();
            }
            let range = GuestRange::new(GuestAddr::new(PAGE), 2 * PAGE).unwrap();
            let source = MapSource::Backing { backing: self.backing, offset: BackingOffset::new(0) };
            map_observed(&mut self.kernel, &mut self.fast, process, range, source, Prot::new(true, true, true)).unwrap();
            let mut model = Pages::default();
            model.map(1, 2, Some((self.backing, 0)), Perm::from_bits(7), PAGE);
            self.spaces.insert(process, model);
            process
        }
    }

    fn world() -> (World, ProcessId) {
        let mut kernel = Kernel::new(InstanceId::new(1), Limits::DEFAULT).unwrap();
        // 最初のプロセスが作る実体の ID を、World の初期値にも使う (すぐ置き換える)。
        let (seed, _) = kernel.create_process(AddrWidth::Bits32, PageSize::new(PAGE).unwrap()).unwrap();
        let backing = kernel.create_backing(seed, PAGE).unwrap();
        let mut world = World { kernel, fast: Fast::new(PAGE).unwrap(), spaces: BTreeMap::new(), backing };
        let process = world.show();
        (world, process)
    }

    #[test]
    fn a_table_that_missed_a_change_is_caught() {
        let (mut w, process) = world();
        let reaches = Expect::Reaches(w.backing, 8);
        assert_eq!(w.fast.access(&w.kernel, process, PAGE + 8, false, reaches), Ok(Reached::At(w.backing, 8)));
        assert_eq!(w.fast.check(&w.spaces, 8), Ok(()));
        // モデルの予想と違う当たり方は、その場で分かる。
        assert!(w.fast.access(&w.kernel, process, PAGE + 8, false, Expect::Reaches(w.backing, 9)).is_err_and(|message| message.starts_with("MEM-4")));
        assert!(w.fast.access(&w.kernel, process, PAGE + 8, false, Expect::Denied).is_err_and(|message| message.starts_with("MEM-4")));
        // 通知なしで外す。表には古い項目が残り、モデルと合わない。
        w.kernel.unmap(process, GuestRange::new(GuestAddr::new(PAGE), 2 * PAGE).unwrap()).unwrap();
        w.spaces.get_mut(&process).unwrap().unmap(1, 2);
        assert!(w.fast.check(&w.spaces, 8).is_err_and(|message| message.starts_with("MEM-4") && message.contains("Unmapped")));
    }

    #[test]
    fn a_write_to_translated_code_through_the_table_is_caught() {
        let (mut w, first) = world();
        let second = w.show();
        let reaches = Expect::Reaches(w.backing, 8);
        // 二つめのプロセスが、速い経路で書けるようになる。
        assert_eq!(w.fast.access(&w.kernel, second, PAGE + 8, true, reaches), Ok(Reached::At(w.backing, 8)));
        assert_eq!(w.fast.check(&w.spaces, 8), Ok(()));
        // 一つめのプロセスの表だけでコードを変換し、二つめの表には知らせない。
        let page = w.fast.tlbs.get_mut(&first).unwrap().protect_code(&w.kernel, &mut w.fast.code, GuestAddr::new(PAGE)).unwrap();
        w.fast.translated.insert((w.backing, 0), page);
        assert!(w.fast.check(&w.spaces, 8).is_err_and(|message| message.starts_with("MEM-4") && message.contains("コード")));
        // 知らせれば、書き込みの側が空いて、モデルと合う。その後の書き込みは速い経路に載らない。
        w.fast.tlbs.get_mut(&second).unwrap().bar_writes(page);
        assert_eq!(w.fast.check(&w.spaces, 8), Ok(()));
        assert_eq!(w.fast.access(&w.kernel, second, PAGE + 8, true, reaches), Ok(Reached::Barred));
        // 書かれた頁として取り出されるのは、書けるようにした頁である。取り出した後は空になる。
        assert_eq!((w.fast.take_dirty(second), w.fast.take_dirty(second), w.fast.take_dirty(first)), (Ok(1), Ok(0), Ok(0)));
        // コードを捨てると、また書ける。
        assert_eq!((w.fast.release_code(0), w.fast.release_code(0)), (Ok(true), Ok(false)));
        assert_eq!(w.fast.access(&w.kernel, second, PAGE + 8, true, reaches), Ok(Reached::At(w.backing, 8)));
    }

    #[test]
    fn a_placement_is_compared_with_the_first_gap_of_the_model() {
        let (mut w, process) = world();
        // 10 頁の領域に、2 頁の実体が一つ。4 頁を二つ置くと埋まり、次は置けない。
        let more: Vec<BackingId> = (0..3).map(|_| w.kernel.create_backing(process, 4 * PAGE).unwrap()).collect();
        assert_eq!((w.fast.backing_created(more[0], 4 * PAGE), w.fast.backing_created(more[1], 4 * PAGE)), (Ok(true), Ok(true)));
        assert_eq!(w.fast.backing_created(more[2], 4 * PAGE), Ok(false));
        // 外すと、その空きに置ける。モデルに無い実体を外すと、位置が合わない。
        assert_eq!(w.fast.backing_reclaimed(more[0]), Ok(()));
        assert_eq!(w.fast.backing_created(more[2], 4 * PAGE), Ok(true));
        w.fast.placed.remove(&more[1]);
        assert!(w.fast.backing_reclaimed(more[1]).is_err_and(|message| message.starts_with("MEM-4")));
        // モデルと違う位置に置いた台帳も分かる。
        w.fast.placed.insert(more[1], (0, PAGE));
        assert!(w.fast.backing_created(more[0], PAGE).is_err_and(|message| message.starts_with("MEM-4")));
    }
}
