//! ゲストのメモリへの速い経路: 実体を一つの線形領域に置く台帳と、変換したコードが直接読む表。
//!
//! 基準の経路 ([`crate::guest_read`]、[`crate::guest_write`]) は、アクセスのたびにマッピングを引く。
//! 変換したコード (JIT) がそれを毎回呼ぶと、実行時間の大半がメモリの呼び出しになる。ここにあるのは、
//! 同じ結果を、表を一度引くだけで得るための部品である。
//!
//! - [`LinearLayout`] は、実体 ([`BackingId`]) を、ホストが持つ一つの連続した領域 (Wasm なら線形メモリ) の
//!   中の位置へ置く台帳である。[`LinearContents`] は、その領域を [`Contents`] として見せる。
//! - [`SoftTlb`] は、プロセスごとの、ゲストの頁から線形領域の中の位置への覚え書きである。項目
//!   ([`TlbEntry`]) の並びは決まっていて (`#[repr(C)]`)、変換したコードは表の番地を埋め込んで直接読む。
//!   外れたら [`SoftTlb::fill`] が基準の経路と同じ判定で埋める。
//! - [`CodePages`] は、変換したコードの元になった実体の頁を持つ。そこへの書き込みは速い経路に載せず、
//!   [`crate::guest_write_watched`] へ落とす (どの別名から書いても、コードを捨てさせるため)。
//!
//! # 変換したコードが行う照合
//!
//! 頁の大きさを `P`、項目の数を `N` (どちらも 2 の冪) とする。アドレス `a` の読み出しは次のとおり。
//! 書き込みは `read` の代わりに `write` を見る。
//!
//! ```
//! /// 項目は (read, write, addend) の三語。当たれば線形領域の中の位置、外れれば `None` (遅い経路)。
//! fn read_position(entries: &[[u32; 3]; 1024], a: u32) -> Option<u32> {
//!     const P: u32 = 4096;
//!     let [read, _write, addend] = entries[usize::try_from(a / P).unwrap() & 1023];
//!     if a & !(P - 1) == read { Some(a.wrapping_add(addend)) } else { None }
//! }
//!
//! // ゲストの 0x5000 の頁が、線形領域の 0x9000 にある。ほかの項目は空 (read が 1)。
//! let mut entries = [[1u32, 1, 0]; 1024];
//! entries[5] = [0x5000, 1, 0x9000u32.wrapping_sub(0x5000)];
//! assert_eq!(read_position(&entries, 0x5123), Some(0x9123));
//! assert_eq!(read_position(&entries, 0x6123), None);
//! // 項目の位置が同じ別の頁 (1024 頁ぶん先) にも当たらない。
//! assert_eq!(read_position(&entries, 0x5123 + 1024 * 4096), None);
//! ```
//!
//! 許可の無い側の値は [`TlbWord::NEVER`] (1) で、頁の先頭の番地 (`P` の倍数、`P >= 2`) とは決して
//! 一致しない。だから一度の比較で、頁の一致と許可を同時に確かめられる。頁をまたぐアクセスは、
//! 変換したコードの側で遅い経路へ落とす。[`SoftTlb::probe`] は、この照合を Rust で書いたものである。
//!
//! # 守ること
//!
//! - 表は、アドレス空間の変更の通知 ([`SpaceObserver`]) で消す。[`crate::map_observed`] などへ
//!   [`SoftTlb`] を受け取り手として渡す。写しから復元した後は [`SoftTlb::flush`] を呼ぶ。
//! - 項目の列は、[`SoftTlb`] を作った時に一度だけ確保し、その後は動かさない。[`SoftTlb`] の値を
//!   移しても、列の番地は変わらない。
//! - 速い経路の結果は、基準の経路と同じでなければならない。決定的な試験が、当たった項目の位置と許可を
//!   毎回 [`Kernel::translate`] と比べる。

use crate::addr::{Access, BackingOffset, GuestAddr, GuestRange, PageSize};
use crate::backend::{Contents, MapChange, SpaceObserver};
use crate::error::{Error, Invariant, Refusal};
use crate::id::{BackingId, ProcessId};
use crate::io::HostError;
use crate::kernel::Kernel;
use crate::mem::{MapGen, MapSource, MemFault};
use crate::store::List;

/// 線形領域の中の位置 (先頭からのバイト数)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct LinearOffset(u64);

impl LinearOffset {
    /// 数値から作る。
    #[must_use]
    pub const fn new(value: u64) -> LinearOffset {
        LinearOffset(value)
    }

    /// 先頭からのバイト数。
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// 実体を線形領域へ置けなかった理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotPlaced {
    /// 連続した空きが足りない。
    Full,
    /// その実体は、もう置いてある。
    Twice,
    /// 長さが 0 か、揃えると表せない。
    BadLength,
    /// 台帳を伸ばせなかった。
    HostMemory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Placed {
    backing: BackingId,
    start: u64,
    /// 実体の長さ。
    len: u64,
    /// 占める長さ (揃えた後)。
    room: u64,
}

/// 実体を、一つの連続した領域の中へ重ならないように置く台帳。内容は持たない。
///
/// 置く位置と占める長さは、作る時に決めた粒度に揃える。空きは先頭から探す。
#[derive(Debug, PartialEq, Eq)]
pub struct LinearLayout {
    capacity: u64,
    align: PageSize,
    /// 位置の昇順。
    placed: List<Placed>,
}

impl LinearLayout {
    /// `capacity` バイトの領域の台帳を作る。位置と長さは `align` に揃える。
    #[must_use]
    pub const fn new(capacity: u64, align: PageSize) -> LinearLayout {
        LinearLayout { capacity, align, placed: List::new() }
    }

    /// 領域の大きさ。
    #[must_use]
    pub const fn capacity(&self) -> u64 {
        self.capacity
    }

    /// `backing` を、長さ `len` で置く。置いた位置を返す。内容は変えない (消すのはホスト、
    /// [`LinearContents::zero`])。
    pub fn place(&mut self, backing: BackingId, len: u64) -> Result<LinearOffset, NotPlaced> {
        if self.find(backing).is_some() {
            return Err(NotPlaced::Twice);
        }
        let mask = self.align.bytes().wrapping_sub(1);
        let room = len.checked_add(mask).map(|padded| padded & !mask).filter(|room| *room != 0).ok_or(NotPlaced::BadLength)?;
        self.placed.reserve(1).map_err(|_| NotPlaced::HostMemory)?;
        // 先頭から、前の実体の終わりと次の実体の始まりの間を見る。
        let mut free_from = 0u64;
        let mut at = 0usize;
        for placed in self.placed.iter() {
            if placed.start.saturating_sub(free_from) >= room {
                break;
            }
            free_from = placed.start.saturating_add(placed.room);
            at = at.saturating_add(1);
        }
        if at == self.placed.len() && self.capacity.saturating_sub(free_from) < room {
            return Err(NotPlaced::Full);
        }
        self.placed.insert(at, Placed { backing, start: free_from, len, room }, Invariant::MemFastPath).map_err(|_| NotPlaced::HostMemory)?;
        Ok(LinearOffset(free_from))
    }

    fn find(&self, backing: BackingId) -> Option<&Placed> {
        self.placed.iter().find(|placed| placed.backing == backing)
    }

    /// `backing` の位置と長さ。置いていなければ `None`。
    #[must_use]
    pub fn locate(&self, backing: BackingId) -> Option<(LinearOffset, u64)> {
        self.find(backing).map(|placed| (LinearOffset(placed.start), placed.len))
    }

    /// `backing` を外す。[`crate::Effect::BackingReclaimed`] を受けたホストが呼ぶ。置いていた位置を返す。
    pub fn release(&mut self, backing: BackingId) -> Option<LinearOffset> {
        self.placed.remove_first(|placed| placed.backing == backing).map(|placed| LinearOffset(placed.start))
    }

    /// 置いてある実体の数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.placed.len()
    }

    /// 何も置いていないか。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.placed.is_empty()
    }

    /// `backing` の `offset` から `len` バイトが、領域のどこにあるか。実体の外へ出るなら `None`。
    fn within(&self, backing: BackingId, offset: BackingOffset, len: usize) -> Option<core::ops::Range<usize>> {
        let placed = self.find(backing)?;
        let len = u64::try_from(len).ok()?;
        let end = offset.get().checked_add(len).filter(|end| *end <= placed.len)?;
        let from = usize::try_from(placed.start.checked_add(offset.get())?).ok()?;
        let until = usize::try_from(placed.start.checked_add(end)?).ok()?;
        Some(from..until)
    }
}

/// [`LinearLayout`] の台帳のとおりに、一つの領域を実体の内容として見せる。
#[derive(Debug)]
pub struct LinearContents<'a> {
    layout: &'a LinearLayout,
    bytes: &'a mut [u8],
}

impl<'a> LinearContents<'a> {
    /// 置いていない実体と、実体や領域の外へ出る範囲に返す番号。
    pub const OUTSIDE: HostError = HostError::new(1);

    /// `layout` のとおりに `bytes` を見せる。
    #[must_use]
    pub const fn new(layout: &'a LinearLayout, bytes: &'a mut [u8]) -> LinearContents<'a> {
        LinearContents { layout, bytes }
    }

    /// `backing` の内容を 0 にする。置いた直後に呼ぶ (前に置いてあった実体の内容が残っている)。
    pub fn zero(&mut self, backing: BackingId) -> Result<(), HostError> {
        let (_, len) = self.layout.locate(backing).ok_or(Self::OUTSIDE)?;
        let len = usize::try_from(len).map_err(|_| Self::OUTSIDE)?;
        let range = self.layout.within(backing, BackingOffset::new(0), len).ok_or(Self::OUTSIDE)?;
        self.bytes.get_mut(range).ok_or(Self::OUTSIDE)?.fill(0);
        Ok(())
    }
}

impl Contents for LinearContents<'_> {
    fn read(&self, backing: BackingId, offset: BackingOffset, out: &mut [u8]) -> Result<(), HostError> {
        let range = self.layout.within(backing, offset, out.len()).ok_or(Self::OUTSIDE)?;
        out.copy_from_slice(self.bytes.get(range).ok_or(Self::OUTSIDE)?);
        Ok(())
    }

    fn write(&mut self, backing: BackingId, offset: BackingOffset, data: &[u8]) -> Result<(), HostError> {
        let range = self.layout.within(backing, offset, data.len()).ok_or(Self::OUTSIDE)?;
        self.bytes.get_mut(range).ok_or(Self::OUTSIDE)?.copy_from_slice(data);
        Ok(())
    }
}

mod sealed {
    pub trait Sealed {}
    impl Sealed for u32 {}
    impl Sealed for u64 {}
}

/// 表の項目の一語。変換したコードの語の幅に合わせて `u32` (Wasm32) か `u64` を選ぶ。
pub trait TlbWord: Copy + Eq + core::fmt::Debug + sealed::Sealed {
    /// 頁の先頭の番地とは決して一致しない値。許可の無い側と、空の項目に入れる。
    const NEVER: Self;
    /// 0。
    const ZERO: Self;

    /// `value` がこの幅に収まれば、その値。
    fn narrow(value: u64) -> Option<Self>;

    /// `u64` へ広げる。
    fn widen(self) -> u64;

    /// 折り返しの足し算。
    #[must_use]
    fn wrapping_add(self, other: Self) -> Self;

    /// 折り返しの引き算。
    #[must_use]
    fn wrapping_sub(self, other: Self) -> Self;
}

impl TlbWord for u32 {
    const NEVER: u32 = 1;
    const ZERO: u32 = 0;

    fn narrow(value: u64) -> Option<u32> {
        u32::try_from(value).ok()
    }

    fn widen(self) -> u64 {
        u64::from(self)
    }

    fn wrapping_add(self, other: u32) -> u32 {
        u32::wrapping_add(self, other)
    }

    fn wrapping_sub(self, other: u32) -> u32 {
        u32::wrapping_sub(self, other)
    }
}

impl TlbWord for u64 {
    const NEVER: u64 = 1;
    const ZERO: u64 = 0;

    fn narrow(value: u64) -> Option<u64> {
        Some(value)
    }

    fn widen(self) -> u64 {
        self
    }

    fn wrapping_add(self, other: u64) -> u64 {
        u64::wrapping_add(self, other)
    }

    fn wrapping_sub(self, other: u64) -> u64 {
        u64::wrapping_sub(self, other)
    }
}

/// 表の一項目。並びは `read`、`write`、`addend` の順で、詰め物は無い (`u32` なら 12 バイト)。
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TlbEntry<W> {
    read: W,
    write: W,
    addend: W,
}

const _: () = assert!(core::mem::size_of::<TlbEntry<u32>>() == 12 && core::mem::align_of::<TlbEntry<u32>>() == 4);
const _: () = assert!(core::mem::size_of::<TlbEntry<u64>>() == 24 && core::mem::align_of::<TlbEntry<u64>>() == 8);
const _: () = assert!(core::mem::offset_of!(TlbEntry<u32>, write) == 4 && core::mem::offset_of!(TlbEntry<u32>, addend) == 8);
const _: () = assert!(core::mem::offset_of!(TlbEntry<u64>, write) == 8 && core::mem::offset_of!(TlbEntry<u64>, addend) == 16);

impl<W: TlbWord> TlbEntry<W> {
    /// どのアドレスにも当たらない項目。
    pub const EMPTY: TlbEntry<W> = TlbEntry { read: W::NEVER, write: W::NEVER, addend: W::ZERO };

    /// 頁の先頭の番地 `tag` を、線形領域の中の位置 `position` へ写す項目。許さない側は当たらない値にする。
    pub(crate) fn filled(tag: W, position: W, readable: bool, writable: bool) -> TlbEntry<W> {
        let read = if readable { tag } else { W::NEVER };
        let write = if writable { tag } else { W::NEVER };
        TlbEntry { read, write, addend: position.wrapping_sub(tag) }
    }

    /// 読める頁の先頭の番地。読めなければ [`TlbWord::NEVER`]。
    #[must_use]
    pub const fn read(&self) -> W {
        self.read
    }

    /// 速い経路で書ける頁の先頭の番地。書けなければ [`TlbWord::NEVER`]。
    #[must_use]
    pub const fn write(&self) -> W {
        self.write
    }

    /// 線形領域の中の位置から、ゲストの番地を引いたもの (折り返し)。
    #[must_use]
    pub const fn addend(&self) -> W {
        self.addend
    }
}

/// `addr` を含む頁の先頭の番地。
pub(crate) const fn page_base(page: PageSize, addr: u64) -> u64 {
    addr & !page.bytes().wrapping_sub(1)
}

/// 変換したコードが行うのと同じ照合。`tag` は項目の、アクセスの種類に合う側の値。`addr` の頁の先頭が
/// `tag` と同じなら、`addr` のバイトの位置を返す。
pub(crate) fn landing<W: TlbWord>(page: PageSize, tag: W, addend: W, addr: u64) -> Option<W> {
    if W::narrow(page_base(page, addr)) != Some(tag) {
        return None;
    }
    Some(W::narrow(addr)?.wrapping_add(addend))
}

/// 実体の中の一つの頁。別名のどこから見ても同じ頁を指す。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BackingPage {
    backing: BackingId,
    at: u64,
}

impl BackingPage {
    /// 実体。
    #[must_use]
    pub const fn backing(&self) -> BackingId {
        self.backing
    }

    /// 頁の先頭の、実体の中の位置。
    #[must_use]
    pub const fn offset(&self) -> BackingOffset {
        BackingOffset::new(self.at)
    }
}

/// 昇順の列へ、無ければ入れる。入れたら `true`。
fn insert_sorted(list: &mut List<BackingPage>, page: BackingPage) -> Result<bool, Error> {
    match list.as_slice().binary_search(&page) {
        Ok(_) => Ok(false),
        Err(at) => {
            list.reserve(1)?;
            list.insert(at, page, Invariant::MemFastPath)?;
            Ok(true)
        }
    }
}

/// 変換したコードの元になった実体の頁の集まり。ここにある頁へは、速い経路で書かせない。
#[derive(Debug, PartialEq, Eq)]
pub struct CodePages {
    pages: List<BackingPage>,
}

impl Default for CodePages {
    fn default() -> CodePages {
        CodePages::new()
    }
}

impl CodePages {
    /// 空の集まり。
    #[must_use]
    pub const fn new() -> CodePages {
        CodePages { pages: List::new() }
    }

    /// `page` を加える。新しく加えたら `true`。加えた後、その実体を見せている全てのプロセスの表で
    /// [`SoftTlb::bar_writes`] を呼ぶ。
    pub fn protect(&mut self, page: BackingPage) -> Result<bool, Error> {
        insert_sorted(&mut self.pages, page)
    }

    /// `page` を外す (その頁のコードを全て捨てた後)。入っていたら `true`。
    pub fn release(&mut self, page: BackingPage) -> bool {
        match self.pages.as_slice().binary_search(&page) {
            Ok(at) => self.pages.remove(at).is_some(),
            Err(_) => false,
        }
    }

    /// `backing` の頁を全て外す ([`crate::Effect::BackingReclaimed`] を受けた後)。
    pub fn forget(&mut self, backing: BackingId) {
        while self.pages.remove_first(|page| page.backing == backing).is_some() {}
    }

    /// `page` が入っているか。
    #[must_use]
    pub fn holds(&self, page: BackingPage) -> bool {
        self.pages.as_slice().binary_search(&page).is_ok()
    }

    /// 入っている頁の数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    /// 空か。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }
}

/// 表を埋められなかった理由。どれも、そのアクセスを基準の経路で行えば正しい結果になる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotFast {
    /// kome の呼び出しの失敗 (古い ID など)。
    Kernel(Error),
    /// ゲストのアクセスの失敗。基準の経路と同じ失敗である。
    Fault(MemFault),
    /// 速い経路に載せない: 実体が線形領域に無い、位置が語の幅に収まらない、コードの頁への書き込み。
    Slow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Origin {
    /// ゲストの頁の先頭の番地。
    guest: u64,
    page: BackingPage,
    /// マッピングが読み出しを許すか。
    readable: bool,
    /// マッピングが書き込みを許すか。
    writable: bool,
}

/// プロセスごとの、ゲストの頁から線形領域の中の位置への覚え書き (直接写像)。
///
/// 項目の列は作る時に一度だけ確保し、動かさない。変換したコードは [`SoftTlb::entries`] の番地を
/// 埋め込んでよい。
#[derive(Debug)]
pub struct SoftTlb<W: TlbWord> {
    process: ProcessId,
    page: PageSize,
    /// 作る時に全ての項目を並べ、その後は数を変えない (番地が動かない)。
    entries: List<TlbEntry<W>>,
    /// 項目ごとの、埋めた元。空の項目は `None`。
    origins: List<Option<Origin>>,
    /// 前に [`SoftTlb::take_dirty`] を呼んでから、速い経路で書けるようにした頁 (昇順)。
    dirty: List<BackingPage>,
}

impl<W: TlbWord> SoftTlb<W> {
    /// `process` の表を、頁の大きさ `page`、項目の数 `slots` で作る。
    ///
    /// `slots` は 2 の冪、`page` は 2 バイト以上でなければならない (`Refused(Misaligned)`)。
    pub fn new(process: ProcessId, page: PageSize, slots: usize) -> Result<SoftTlb<W>, Error> {
        if !slots.is_power_of_two() || page.bytes() < 2 {
            return Err(Error::Refused(Refusal::Misaligned));
        }
        Ok(SoftTlb { process, page, entries: List::filled(TlbEntry::EMPTY, slots)?, origins: List::filled(None, slots)?, dirty: List::new() })
    }

    /// このプロセスの表である。
    #[must_use]
    pub const fn process(&self) -> ProcessId {
        self.process
    }

    /// 頁の大きさ。
    #[must_use]
    pub const fn page(&self) -> PageSize {
        self.page
    }

    /// 項目の列。変換したコードは、この列の番地と [`TlbEntry`] の並びを使って直接読む。
    #[must_use]
    pub fn entries(&self) -> &[TlbEntry<W>] {
        self.entries.as_slice()
    }

    /// `addr` を含む頁の先頭の番地。
    const fn base(&self, addr: u64) -> u64 {
        page_base(self.page, addr)
    }

    /// `addr` の項目の位置: 頁の番号の下位。
    fn slot(&self, addr: u64) -> usize {
        let number = addr.wrapping_shr(self.page.bytes().trailing_zeros());
        usize::try_from(number).unwrap_or(usize::MAX) & self.entries.len().wrapping_sub(1)
    }

    /// 変換したコードが行うのと同じ照合。当たれば、`addr` のバイトの線形領域の中の位置を返す。
    /// 実行のためのアクセスは、表では扱わない。
    #[must_use]
    pub fn probe(&self, addr: GuestAddr, access: Access) -> Option<LinearOffset> {
        let entry = self.entries.get(self.slot(addr.get()))?;
        let tag = match access {
            Access::Read => entry.read,
            Access::Write => entry.write,
            Access::Execute => return None,
        };
        landing(self.page, tag, entry.addend, addr.get()).map(|position| LinearOffset(position.widen()))
    }

    /// `addr` を含む頁の項目を、基準の経路と同じ判定で埋める。`access` が許されなければ、基準の経路と
    /// 同じ失敗を返し、表は変えない。
    ///
    /// 書き込みで埋めた頁は、書かれたものとして覚える ([`SoftTlb::take_dirty`])。読み出しで埋めた
    /// 項目は、その頁がまだ書かれていなければ、書き込みの側を空けておく (最初の書き込みがここへ来る)。
    pub fn fill(&mut self, kernel: &Kernel, layout: &LinearLayout, code: &CodePages, addr: GuestAddr, access: Access) -> Result<TlbEntry<W>, NotFast> {
        let (origin, position) = self.resolve(kernel, layout, addr, access)?;
        let tag = W::narrow(origin.guest).ok_or(NotFast::Slow)?;
        let last = position.checked_add(self.page.bytes().wrapping_sub(1)).ok_or(NotFast::Slow)?;
        let (Some(position), Some(_)) = (W::narrow(position), W::narrow(last)) else { return Err(NotFast::Slow) };
        let barred = code.holds(origin.page);
        if access == Access::Write {
            if barred {
                return Err(NotFast::Slow);
            }
            insert_sorted(&mut self.dirty, origin.page).map_err(|_| NotFast::Slow)?;
        }
        let written = self.dirty.as_slice().binary_search(&origin.page).is_ok();
        let entry = TlbEntry::filled(tag, position, origin.readable, origin.writable && !barred && written);
        let slot = self.slot(addr.get());
        if let (Some(kept), Some(from)) = (self.entries.get_mut(slot), self.origins.get_mut(slot)) {
            *kept = entry;
            *from = Some(origin);
        }
        Ok(entry)
    }

    /// `addr` を含む頁の元と、頁の先頭の線形領域の中の位置。
    fn resolve(&self, kernel: &Kernel, layout: &LinearLayout, addr: GuestAddr, access: Access) -> Result<(Origin, u64), NotFast> {
        let one = GuestRange::new(addr, 1).ok_or(NotFast::Slow)?;
        let span = kernel.translate(self.process, one, access).map_err(NotFast::Kernel)?.map_err(NotFast::Fault)?;
        let mapping = kernel.mapping_at(self.process, addr).map_err(NotFast::Kernel)?.ok_or(NotFast::Slow)?;
        let guest = self.base(addr.get());
        let into = addr.get().wrapping_sub(guest);
        // 頁の全体が、一つのマッピングと一つの実体の中にある。
        let whole = GuestRange::new(GuestAddr::new(guest), self.page.bytes()).ok_or(NotFast::Slow)?;
        if !mapping.range().contains(whole.start()) || !mapping.range().contains(whole.last()) {
            return Err(NotFast::Slow);
        }
        let MapSource::Backing { backing, .. } = mapping.source() else { return Err(NotFast::Slow) };
        let at = span.offset().get().checked_sub(into).ok_or(NotFast::Slow)?;
        let (start, len) = layout.locate(backing).ok_or(NotFast::Slow)?;
        if at.checked_add(self.page.bytes()).is_none_or(|end| end > len) {
            return Err(NotFast::Slow);
        }
        let position = start.get().checked_add(at).ok_or(NotFast::Slow)?;
        let prot = mapping.prot();
        let origin = Origin { guest, page: BackingPage { backing, at }, readable: prot.allows(Access::Read), writable: prot.allows(Access::Write) };
        Ok((origin, position))
    }

    /// `addr` のコードを変換する前に呼ぶ。`addr` を含む実体の頁を `code` に加え、この表の書き込みの
    /// 側を空ける。返した頁を見せている他のプロセスの表には、[`SoftTlb::bar_writes`] を呼ぶ。
    pub fn protect_code(&mut self, kernel: &Kernel, code: &mut CodePages, addr: GuestAddr) -> Result<BackingPage, NotFast> {
        let one = GuestRange::new(addr, 1).ok_or(NotFast::Slow)?;
        let span = kernel.translate(self.process, one, Access::Execute).map_err(NotFast::Kernel)?.map_err(NotFast::Fault)?;
        let at = span.offset().get() & !self.page.bytes().wrapping_sub(1);
        let page = BackingPage { backing: span.backing(), at };
        code.protect(page).map_err(NotFast::Kernel)?;
        self.bar_writes(page);
        Ok(page)
    }

    /// `page` を写している項目の、書き込みの側を空ける。読み出しの側は残す。
    pub fn bar_writes(&mut self, page: BackingPage) {
        for (entry, origin) in self.entries.iter_mut().zip(self.origins.iter()) {
            if origin.is_some_and(|origin| origin.page == page) {
                entry.write = W::NEVER;
            }
        }
    }

    /// 前に呼んでから速い経路で書けるようにした頁を `visit` へ渡し、覚えを消す。全ての項目の書き込みの
    /// 側を空けるので、次の書き込みはもう一度 [`SoftTlb::fill`] へ来る。
    ///
    /// 遅い経路 ([`crate::guest_write`]) で書いた範囲は、ここには入らない (呼んだホストが知っている)。
    pub fn take_dirty(&mut self, mut visit: impl FnMut(BackingPage)) {
        while let Some(page) = self.dirty.pop() {
            visit(page);
        }
        for entry in self.entries.iter_mut() {
            entry.write = W::NEVER;
        }
    }

    /// 全ての項目を空にする。写しから復元した後と、線形領域の中で実体を動かした後に呼ぶ。
    /// 書かれた頁の覚えは残す。
    pub fn flush(&mut self) {
        for (entry, origin) in self.entries.iter_mut().zip(self.origins.iter_mut()) {
            *entry = TlbEntry::EMPTY;
            *origin = None;
        }
    }

    /// `range` に重なる頁の項目を空にする。
    fn drop_range(&mut self, range: GuestRange) {
        let (first, last) = (self.base(range.start().get()), self.base(range.last().get()));
        for (entry, origin) in self.entries.iter_mut().zip(self.origins.iter_mut()) {
            if origin.is_some_and(|origin| first <= origin.guest && origin.guest <= last) {
                *entry = TlbEntry::EMPTY;
                *origin = None;
            }
        }
    }
}

impl<W: TlbWord> SpaceObserver for SoftTlb<W> {
    fn changed(&mut self, process: ProcessId, _generation: MapGen, change: MapChange) {
        if process != self.process {
            return;
        }
        match change {
            // 空いていた範囲に、埋めた項目は無い。
            MapChange::Mapped { .. } => {}
            MapChange::Unmapped { range } | MapChange::Protected { range, .. } => self.drop_range(range),
        }
    }

    fn cleared(&mut self, process: ProcessId) {
        if process == self.process {
            self.flush();
            while self.dirty.pop().is_some() {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::{AddrWidth, Prot};
    use crate::backend::{map_observed, protect_observed, unmap_observed};
    use crate::kernel::InstanceId;
    use crate::limits::Limits;
    use crate::mem::MemFaultKind;

    const PAGE: u64 = 0x1000;

    fn page() -> PageSize {
        PageSize::new(PAGE).unwrap()
    }

    fn range(start: u64, len: u64) -> GuestRange {
        GuestRange::new(GuestAddr::new(start), len).unwrap()
    }

    /// プロセス一つと、線形領域に置いた 4 頁の実体一つ。
    fn world() -> (Kernel, ProcessId, BackingId, LinearLayout) {
        let mut kernel = Kernel::new(InstanceId::new(1), Limits::DEFAULT).unwrap();
        let (process, _) = kernel.create_process(AddrWidth::Bits32, page()).unwrap();
        let backing = kernel.create_backing(process, 4 * PAGE).unwrap();
        let mut layout = LinearLayout::new(16 * PAGE, page());
        // 先に別の実体を置いて、位置が 0 にならないようにする。
        let filler = kernel.create_backing(process, 2 * PAGE).unwrap();
        assert_eq!(layout.place(filler, 2 * PAGE), Ok(LinearOffset::new(0)));
        assert_eq!(layout.place(backing, 4 * PAGE), Ok(LinearOffset::new(2 * PAGE)));
        (kernel, process, backing, layout)
    }

    fn backed(backing: BackingId, offset: u64) -> MapSource {
        MapSource::Backing { backing, offset: BackingOffset::new(offset) }
    }

    #[test]
    fn the_layout_places_first_fit_on_the_alignment_and_reuses_a_gap() {
        let mut kernel = Kernel::new(InstanceId::new(1), Limits::DEFAULT).unwrap();
        let (process, _) = kernel.create_process(AddrWidth::Bits32, page()).unwrap();
        let ids: Vec<BackingId> = (0..5).map(|_| kernel.create_backing(process, PAGE).unwrap()).collect();
        let mut layout = LinearLayout::new(8 * PAGE, page());
        assert!(layout.is_empty());
        assert_eq!(layout.capacity(), 8 * PAGE);
        // 長さは粒度に切り上げて場所を取る。実体の長さそのものは覚えている。
        assert_eq!(layout.place(ids[0], 1), Ok(LinearOffset::new(0)));
        assert!(!layout.is_empty());
        assert_eq!(layout.place(ids[1], 2 * PAGE + 1), Ok(LinearOffset::new(PAGE)));
        assert_eq!(layout.place(ids[2], PAGE), Ok(LinearOffset::new(4 * PAGE)));
        assert_eq!(layout.locate(ids[1]), Some((LinearOffset::new(PAGE), 2 * PAGE + 1)));
        assert_eq!((layout.place(ids[1], PAGE), layout.place(ids[3], 0), layout.place(ids[3], u64::MAX)), (Err(NotPlaced::Twice), Err(NotPlaced::BadLength), Err(NotPlaced::BadLength)));
        // 残りは 3 頁。4 頁は置けず、置けなかった台帳は変わらない。
        assert_eq!(layout.place(ids[3], 3 * PAGE + 1), Err(NotPlaced::Full));
        assert_eq!((layout.len(), layout.locate(ids[3])), (3, None));
        // 外した所の空きに収まるものは、そこへ入る。収まらないものは後ろへ行く。
        assert_eq!(layout.release(ids[1]), Some(LinearOffset::new(PAGE)));
        assert_eq!(layout.release(ids[1]), None);
        assert_eq!(layout.place(ids[3], 3 * PAGE), Ok(LinearOffset::new(PAGE)));
        assert_eq!(layout.place(ids[4], 3 * PAGE), Ok(LinearOffset::new(5 * PAGE)));
        assert_eq!(layout.release(ids[3]), Some(LinearOffset::new(PAGE)));
        assert_eq!(layout.place(ids[1], 4 * PAGE), Err(NotPlaced::Full));
        assert_eq!(layout.place(ids[1], 3 * PAGE), Ok(LinearOffset::new(PAGE)));
    }

    #[test]
    fn linear_contents_read_and_write_inside_the_backing_only() {
        let (_kernel, _process, backing, layout) = world();
        let mut bytes = vec![0xEEu8; 16 * 0x1000];
        let mut contents = LinearContents::new(&layout, &mut bytes);
        contents.zero(backing).unwrap();
        contents.write(backing, BackingOffset::new(PAGE - 1), &[1, 2, 3]).unwrap();
        let mut out = [9u8; 5];
        contents.read(backing, BackingOffset::new(PAGE - 2), &mut out).unwrap();
        assert_eq!(out, [0, 1, 2, 3, 0]);
        // 実体の外へ出る範囲と、置いていない実体は断る。何も変えない。
        let outside = Err(LinearContents::OUTSIDE);
        assert_eq!(contents.read(backing, BackingOffset::new(4 * PAGE - 1), &mut [0u8; 2]), outside);
        assert_eq!(contents.write(backing, BackingOffset::new(4 * PAGE), &[1]), outside);
        assert_eq!(contents.write(backing, BackingOffset::new(u64::MAX), &[1]), outside);
        let mut other = LinearLayout::new(16 * PAGE, page());
        let _ = &mut other;
        let mut elsewhere = vec![0u8; 16];
        let mut unplaced = LinearContents::new(&other, &mut elsewhere);
        assert_eq!((unplaced.zero(backing), unplaced.read(backing, BackingOffset::new(0), &mut [0u8; 1])), (outside, outside));
        // 領域が台帳より短ければ、領域の外も断る。
        let mut short = vec![0u8; 0x2800];
        let mut cut = LinearContents::new(&layout, &mut short);
        assert_eq!((cut.zero(backing), cut.write(backing, BackingOffset::new(PAGE), &[1])), (outside, outside));
        cut.write(backing, BackingOffset::new(0), &[7]).unwrap();
        // 書いたものは、線形領域の「位置 + 実体の中の位置」にある。前の実体の領域には触れていない。
        assert_eq!((bytes[0x2000 + 0xFFF], bytes[0x2000 + 0x1000], bytes[0x2000 + 0x1001], bytes[0x1FFF]), (1, 2, 3, 0xEE));
    }

    #[test]
    fn a_filled_page_is_hit_with_the_position_of_the_reference_path() {
        let (mut kernel, process, backing, layout) = world();
        let code = CodePages::new();
        let mut tlb: SoftTlb<u32> = SoftTlb::new(process, page(), 4).unwrap();
        assert_eq!((tlb.process(), tlb.page(), tlb.entries().len()), (process, page(), 4));
        // 実体の 2 頁めから 2 頁ぶんを、0x7000 へ読み書きできるように見せる。
        map_observed(&mut kernel, &mut tlb, process, range(0x7000, 2 * PAGE), backed(backing, PAGE), Prot::READ_WRITE).unwrap();
        let addr = GuestAddr::new(0x8123);
        assert_eq!((tlb.probe(addr, Access::Read), tlb.probe(addr, Access::Write)), (None, None));
        // 読み出しで埋めると、読み出しだけが当たる。位置は 線形領域の位置 + 実体の中の位置。
        let entry = tlb.fill(&kernel, &layout, &code, addr, Access::Read).unwrap();
        let position = LinearOffset::new(2 * PAGE + 2 * PAGE + 0x123);
        assert_eq!((entry.read(), entry.write(), entry.addend()), (0x8000, u32::NEVER, 0x4000u32.wrapping_sub(0x8000)));
        assert_eq!((tlb.probe(addr, Access::Read), tlb.probe(addr, Access::Write), tlb.probe(addr, Access::Execute)), (Some(position), None, None));
        assert_eq!(tlb.entries().first(), Some(&entry));
        // 最初の書き込みで埋め直すと、書き込みも当たり、頁が「書かれた」に入る。
        let entry = tlb.fill(&kernel, &layout, &code, addr, Access::Write).unwrap();
        assert_eq!((entry.read(), entry.write()), (0x8000, 0x8000));
        assert_eq!(tlb.probe(addr, Access::Write), Some(position));
        // 同じ頁のほかのバイトにも当たり、隣の頁と、項目の位置が同じ別の頁には当たらない。
        assert_eq!(tlb.probe(GuestAddr::new(0x8FFF), Access::Read), Some(LinearOffset::new(0x4FFF)));
        assert_eq!((tlb.probe(GuestAddr::new(0x7FFF), Access::Read), tlb.probe(GuestAddr::new(0xC123), Access::Read)), (None, None));
        // 書かれた頁を取り出すと、書き込みの側が空く。読み出しは当たり続ける。
        let mut seen = Vec::new();
        tlb.take_dirty(|page| seen.push((page.backing(), page.offset())));
        assert_eq!(seen, vec![(backing, BackingOffset::new(2 * PAGE))]);
        assert_eq!((tlb.probe(addr, Access::Read), tlb.probe(addr, Access::Write)), (Some(position), None));
        tlb.take_dirty(|_| panic!("取り出した後に、書かれた頁が残っている"));
    }

    #[test]
    fn a_fill_fails_exactly_as_the_reference_path_does() {
        let (mut kernel, process, backing, layout) = world();
        let code = CodePages::new();
        let mut tlb: SoftTlb<u32> = SoftTlb::new(process, page(), 8).unwrap();
        map_observed(&mut kernel, &mut tlb, process, range(0x1000, PAGE), backed(backing, 0), Prot::READ).unwrap();
        map_observed(&mut kernel, &mut tlb, process, range(0x2000, PAGE), MapSource::Reserved, Prot::NONE).unwrap();
        for (addr, access, kind) in [
            (0x1010, Access::Write, MemFaultKind::Denied),
            (0x1010, Access::Execute, MemFaultKind::Denied),
            (0x2010, Access::Read, MemFaultKind::Reserved),
            (0x3010, Access::Read, MemFaultKind::Unmapped),
        ] {
            let addr = GuestAddr::new(addr);
            let reference = kernel.translate(process, range(addr.get(), 1), access).unwrap().unwrap_err();
            assert_eq!(reference, MemFault::new(addr, kind));
            assert_eq!(tlb.fill(&kernel, &layout, &code, addr, access), Err(NotFast::Fault(reference)));
        }
        assert!(tlb.entries().iter().all(|entry| *entry == TlbEntry::EMPTY));
        // 読めるが書けない頁は、読み出しの側だけが埋まる。書かれた頁にもならない。
        tlb.fill(&kernel, &layout, &code, GuestAddr::new(0x1010), Access::Read).unwrap();
        assert_eq!((tlb.probe(GuestAddr::new(0x1010), Access::Read).is_some(), tlb.probe(GuestAddr::new(0x1010), Access::Write)), (true, None));
        tlb.take_dirty(|_| panic!("書けない頁が、書かれた頁に入った"));
        // 古い ID のプロセスの表は、kome の失敗を返す。
        let stale: SoftTlb<u32> = SoftTlb::new(process, page(), 8).unwrap();
        let mut gone = stale;
        kernel.exit_process(process, crate::process::ExitCode::new(0)).unwrap();
        while kernel.poll_effect().is_some() {}
        assert!(matches!(gone.fill(&kernel, &layout, &code, GuestAddr::new(0x1010), Access::Read), Err(NotFast::Kernel(Error::Stale(_)))));
        assert!(matches!(gone.protect_code(&kernel, &mut CodePages::new(), GuestAddr::new(0x1010)), Err(NotFast::Kernel(Error::Stale(_)))));
    }

    #[test]
    fn what_the_table_cannot_hold_goes_to_the_slow_path() {
        let (mut kernel, process, backing, mut layout) = world();
        let code = CodePages::new();
        let mut tlb: SoftTlb<u32> = SoftTlb::new(process, page(), 8).unwrap();
        // 線形領域に置いていない実体。
        let loose = kernel.create_backing(process, PAGE).unwrap();
        map_observed(&mut kernel, &mut tlb, process, range(0x1000, PAGE), backed(loose, 0), Prot::READ).unwrap();
        assert_eq!(tlb.fill(&kernel, &layout, &code, GuestAddr::new(0x1000), Access::Read), Err(NotFast::Slow));
        // 実体の終わりが頁の途中にある (頁の全体が実体の中に無い)。
        let odd = kernel.create_backing(process, PAGE).unwrap();
        layout.place(odd, PAGE / 2).unwrap();
        map_observed(&mut kernel, &mut tlb, process, range(0x2000, PAGE), backed(odd, 0), Prot::READ).unwrap();
        assert_eq!(tlb.fill(&kernel, &layout, &code, GuestAddr::new(0x2000), Access::Read), Err(NotFast::Slow));
        // 位置が 32 ビットに収まらない線形領域。64 ビットの語なら載る。
        let mut far = LinearLayout::new(u64::MAX, PageSize::new(1 << 32).unwrap());
        let pad = kernel.create_backing(process, PAGE).unwrap();
        far.place(pad, 1).unwrap();
        far.place(backing, 4 * PAGE).unwrap();
        map_observed(&mut kernel, &mut tlb, process, range(0x3000, PAGE), backed(backing, 0), Prot::READ).unwrap();
        assert_eq!(tlb.fill(&kernel, &far, &code, GuestAddr::new(0x3000), Access::Read), Err(NotFast::Slow));
        let mut wide: SoftTlb<u64> = SoftTlb::new(process, page(), 8).unwrap();
        wide.fill(&kernel, &far, &code, GuestAddr::new(0x3004), Access::Read).unwrap();
        assert_eq!(wide.probe(GuestAddr::new(0x3004), Access::Read), Some(LinearOffset::new((1 << 32) + 4)));
        // 頁の終わりだけが 32 ビットを越える位置も載せない。
        let mut edge = LinearLayout::new(u64::MAX, page());
        edge.place(pad, (1 << 32) - PAGE - PAGE / 2).unwrap();
        edge.place(backing, 4 * PAGE).unwrap();
        assert_eq!(edge.locate(backing).map(|(start, _)| start.get()), Some((1 << 32) - PAGE));
        tlb.fill(&kernel, &edge, &code, GuestAddr::new(0x3000), Access::Read).unwrap();
        map_observed(&mut kernel, &mut tlb, process, range(0x4000, PAGE), backed(backing, PAGE), Prot::READ).unwrap();
        assert_eq!(tlb.fill(&kernel, &edge, &code, GuestAddr::new(0x4000), Access::Read), Err(NotFast::Slow));
        assert!(tlb.entries().iter().filter(|entry| **entry != TlbEntry::EMPTY).count() == 1);
        // 表の頁がプロセスの頁より大きいとき、頁の全体が一つのマッピングの中に無ければ載せない
        // (前半だけ、後半だけ)。全体が一つのマッピングの中にあれば載る。
        let mut big: SoftTlb<u32> = SoftTlb::new(process, PageSize::new(2 * PAGE).unwrap(), 8).unwrap();
        map_observed(&mut kernel, &mut big, process, range(0x10000, PAGE), backed(backing, 0), Prot::READ).unwrap();
        map_observed(&mut kernel, &mut big, process, range(0x13000, PAGE), backed(backing, PAGE), Prot::READ).unwrap();
        map_observed(&mut kernel, &mut big, process, range(0x14000, 2 * PAGE), backed(backing, 2 * PAGE), Prot::READ).unwrap();
        assert_eq!(big.fill(&kernel, &layout, &code, GuestAddr::new(0x10000), Access::Read), Err(NotFast::Slow));
        assert_eq!(big.fill(&kernel, &layout, &code, GuestAddr::new(0x13000), Access::Read), Err(NotFast::Slow));
        big.fill(&kernel, &layout, &code, GuestAddr::new(0x15008), Access::Read).unwrap();
        assert_eq!(big.probe(GuestAddr::new(0x14008), Access::Read), Some(LinearOffset::new(2 * PAGE + 2 * PAGE + 8)));
        // 形の合わない表は作らない。
        for (page, slots) in [(page(), 0), (page(), 3), (PageSize::new(1).unwrap(), 4)] {
            assert_eq!(SoftTlb::<u32>::new(process, page, slots).err(), Some(Error::Refused(Refusal::Misaligned)));
        }
        assert!(SoftTlb::<u64>::new(process, PageSize::new(2).unwrap(), 1).is_ok());
    }

    #[test]
    fn a_change_of_the_address_space_empties_exactly_the_pages_it_covers() {
        let (mut kernel, process, backing, layout) = world();
        let code = CodePages::new();
        let mut tlb: SoftTlb<u32> = SoftTlb::new(process, page(), 8).unwrap();
        map_observed(&mut kernel, &mut tlb, process, range(0x1000, 4 * PAGE), backed(backing, 0), Prot::READ_WRITE).unwrap();
        let pages = [0x1000u64, 0x2000, 0x3000, 0x4000];
        let hits = |tlb: &SoftTlb<u32>, access| pages.map(|at| tlb.probe(GuestAddr::new(at), access).is_some());
        let fill_all = |tlb: &mut SoftTlb<u32>, kernel: &Kernel| {
            for at in pages {
                tlb.fill(kernel, &layout, &code, GuestAddr::new(at), Access::Write).unwrap();
            }
        };
        fill_all(&mut tlb, &kernel);
        assert_eq!((hits(&tlb, Access::Read), hits(&tlb, Access::Write)), ([true; 4], [true; 4]));
        // 別のプロセスの変更は、この表に関係しない。
        let (other, _) = kernel.create_process(AddrWidth::Bits32, page()).unwrap();
        map_observed(&mut kernel, &mut tlb, other, range(0x2000, PAGE), MapSource::Reserved, Prot::NONE).unwrap();
        unmap_observed(&mut kernel, &mut tlb, other, range(0x2000, PAGE)).unwrap();
        tlb.cleared(other);
        assert_eq!(hits(&tlb, Access::Read), [true; 4]);
        // 権限を変えた頁だけが空になる。埋め直すと、新しい権限になる。
        protect_observed(&mut kernel, &mut tlb, process, range(0x2000, 2 * PAGE), Prot::READ).unwrap();
        assert_eq!(hits(&tlb, Access::Read), [true, false, false, true]);
        tlb.fill(&kernel, &layout, &code, GuestAddr::new(0x2000), Access::Read).unwrap();
        assert_eq!((hits(&tlb, Access::Read), hits(&tlb, Access::Write)), ([true, true, false, true], [true, false, false, true]));
        // 外した頁だけが空になる。
        unmap_observed(&mut kernel, &mut tlb, process, range(0x1000, PAGE)).unwrap();
        assert_eq!(hits(&tlb, Access::Read), [false, true, false, true]);
        // 全て空にしても、書かれた頁の覚えは残る。プロセスが無くなれば、それも消える。
        tlb.flush();
        assert_eq!(hits(&tlb, Access::Read), [false; 4]);
        let mut dirty = 0usize;
        tlb.take_dirty(|_| dirty = dirty.saturating_add(1));
        assert_eq!(dirty, 4);
        protect_observed(&mut kernel, &mut tlb, process, range(0x2000, 2 * PAGE), Prot::READ_WRITE).unwrap();
        map_observed(&mut kernel, &mut tlb, process, range(0x1000, PAGE), backed(backing, 0), Prot::READ_WRITE).unwrap();
        fill_all(&mut tlb, &kernel);
        tlb.cleared(process);
        assert_eq!(hits(&tlb, Access::Read), [false; 4]);
        tlb.take_dirty(|_| panic!("無くなったプロセスの、書かれた頁が残っている"));
    }

    #[test]
    fn a_page_holding_translated_code_is_never_written_through_the_table() {
        let (mut kernel, process, backing, layout) = world();
        let mut code = CodePages::default();
        assert_eq!((code.len(), code.is_empty()), (0, true));
        let mut tlb: SoftTlb<u32> = SoftTlb::new(process, page(), 8).unwrap();
        // 同じ実体の頁を、実行できる範囲 (0x1000) と書ける範囲 (0x5000) の二か所に見せる。
        map_observed(&mut kernel, &mut tlb, process, range(0x1000, PAGE), backed(backing, PAGE), Prot::READ_EXECUTE).unwrap();
        map_observed(&mut kernel, &mut tlb, process, range(0x5000, 2 * PAGE), backed(backing, PAGE), Prot::READ_WRITE).unwrap();
        let alias = GuestAddr::new(0x5008);
        tlb.fill(&kernel, &layout, &code, alias, Access::Write).unwrap();
        tlb.fill(&kernel, &layout, &code, GuestAddr::new(0x6008), Access::Write).unwrap();
        assert!(tlb.probe(alias, Access::Write).is_some());
        // 0x1000 のコードを変換する。別名の書き込みの側が空き、埋め直しも遅い経路へ落ちる。
        let page = tlb.protect_code(&kernel, &mut code, GuestAddr::new(0x1234)).unwrap();
        assert_eq!((page.backing(), page.offset(), code.len(), code.holds(page)), (backing, BackingOffset::new(PAGE), 1, true));
        assert!(!code.is_empty());
        assert_eq!((tlb.probe(alias, Access::Write), tlb.probe(alias, Access::Read).is_some()), (None, true));
        assert_eq!(tlb.fill(&kernel, &layout, &code, alias, Access::Write), Err(NotFast::Slow));
        assert!(tlb.fill(&kernel, &layout, &code, alias, Access::Read).is_ok_and(|entry| entry.write() == u32::NEVER));
        // 同じ実体の隣の頁は、書けるまま。
        assert!(tlb.probe(GuestAddr::new(0x6008), Access::Write).is_some());
        // 実行できない場所のコードは変換できない。集まりも変わらない。
        assert!(matches!(tlb.protect_code(&kernel, &mut code, alias), Err(NotFast::Fault(_))));
        assert_eq!((code.protect(page), code.len()), (Ok(false), 1));
        // コードを捨てて頁を外すと、また速い経路で書ける。
        assert_eq!((code.release(page), code.release(page), code.is_empty()), (true, false, true));
        tlb.fill(&kernel, &layout, &code, alias, Access::Write).unwrap();
        assert!(tlb.probe(alias, Access::Write).is_some());
        // 実体ごと忘れる口は、その実体の頁だけを外す。
        let other = kernel.create_backing(process, PAGE).unwrap();
        map_observed(&mut kernel, &mut tlb, process, range(0x9000, PAGE), backed(other, 0), Prot::READ_EXECUTE).unwrap();
        tlb.protect_code(&kernel, &mut code, GuestAddr::new(0x1000)).unwrap();
        let kept = tlb.protect_code(&kernel, &mut code, GuestAddr::new(0x9000)).unwrap();
        assert_eq!(code.len(), 2);
        code.forget(backing);
        assert_eq!((code.len(), code.holds(kept), code.holds(page)), (1, true, false));
    }
}
