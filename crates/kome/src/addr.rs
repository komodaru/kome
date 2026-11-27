//! ゲストのアドレスと、混同してはいけない値の型。
//!
//! ゲスト仮想アドレス、実体の中の位置、資源 ID は別の型である。kome はホストの
//! ポインタを扱わないので、ゲストアドレスを `usize` へ変換する入口を持たない。
//!
//! ゲストアドレスはホストの `usize` にならない。下の二つは変換先の型だけが違う。
//!
//! ```
//! let _: kome::GuestAddr = kome::GuestAddr::new(0x1000).into();
//! ```
//!
//! ```compile_fail
//! let _: usize = kome::GuestAddr::new(0x1000).into();
//! ```
//!
//! ゲストアドレスと実体の中の位置は比べられない。下の二つは右辺の型だけが違う。
//!
//! ```
//! let _ = kome::GuestAddr::new(0) == kome::GuestAddr::new(0);
//! ```
//!
//! ```compile_fail
//! let _ = kome::GuestAddr::new(0) == kome::BackingOffset::new(0);
//! ```

/// ゲストが見る仮想アドレス。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(flux, flux::refined_by(addr: int))]
pub struct GuestAddr(#[cfg_attr(flux, flux::field(u64[addr]))] u64);

impl GuestAddr {
    /// 数値から作る。
    #[must_use]
    #[cfg_attr(flux, flux::spec(fn(value: u64) -> GuestAddr[value]))]
    pub const fn new(value: u64) -> GuestAddr {
        GuestAddr(value)
    }

    /// ゲストのアドレスとしての数値。
    #[must_use]
    #[cfg_attr(flux, flux::spec(fn(GuestAddr[@addr]) -> u64[addr]))]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// メモリの実体の先頭からの位置。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BackingOffset(u64);

impl BackingOffset {
    /// 数値から作る。
    #[must_use]
    pub const fn new(value: u64) -> BackingOffset {
        BackingOffset(value)
    }

    /// 実体の先頭からのバイト数。
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// 空でなく、`u64` を回り込まないゲストアドレスの範囲。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(flux, flux::refined_by(start: int, last: int))]
#[cfg_attr(flux, flux::invariant(start <= last))]
pub struct GuestRange {
    #[cfg_attr(flux, flux::field(u64[start]))]
    start: u64,
    /// 最後のバイトのアドレス (`start` 以上)。
    #[cfg_attr(flux, flux::field({u64[last] | start <= last}))]
    last: u64,
}

impl GuestRange {
    /// `start` から `len` バイト。`len` が 0、または末尾が `u64` を超えるなら `None`。
    #[must_use]
    #[cfg_attr(flux, flux::spec(fn(GuestAddr[@start], len: u64) -> Option<GuestRange[start, start + len - 1]>[len > 0 && start + len - 1 <= u64::MAX]))]
    pub const fn new(start: GuestAddr, len: u64) -> Option<GuestRange> {
        let Some(tail) = len.checked_sub(1) else { return None };
        match start.0.checked_add(tail) {
            Some(last) => Some(GuestRange { start: start.0, last }),
            None => None,
        }
    }

    /// 最初と最後のバイトのアドレスから作る。`last < first` なら `None`。
    #[must_use]
    #[cfg_attr(flux, flux::spec(fn(GuestAddr[@first], GuestAddr[@last]) -> Option<GuestRange[first, last]>[first <= last]))]
    pub const fn spanning(first: GuestAddr, last: GuestAddr) -> Option<GuestRange> {
        if last.0 < first.0 { None } else { Some(GuestRange { start: first.0, last: last.0 }) }
    }

    /// 先頭のアドレス。
    #[must_use]
    #[cfg_attr(flux, flux::spec(fn(GuestRange[@range]) -> GuestAddr[range.start]))]
    pub const fn start(self) -> GuestAddr {
        GuestAddr(self.start)
    }

    /// 最後のバイトのアドレス。
    #[must_use]
    #[cfg_attr(flux, flux::spec(fn(GuestRange[@range]) -> GuestAddr[range.last]))]
    pub const fn last(self) -> GuestAddr {
        GuestAddr(self.last)
    }

    /// バイト数。範囲が `u64` 全体のときだけ `None` (2^64 は表現できない)。
    #[must_use]
    #[cfg_attr(flux, flux::spec(fn(GuestRange[@range]) -> Option<u64[range.last - range.start + 1]>[range.last - range.start < u64::MAX]))]
    pub const fn byte_len(self) -> Option<u64> {
        self.last.wrapping_sub(self.start).checked_add(1)
    }

    /// `addr` を含むか。
    #[must_use]
    #[cfg_attr(flux, flux::spec(fn(GuestRange[@range], GuestAddr[@addr]) -> bool[range.start <= addr && addr <= range.last]))]
    pub const fn contains(self, addr: GuestAddr) -> bool {
        self.start <= addr.0 && addr.0 <= self.last
    }

    /// 一バイトでも重なるか。
    #[must_use]
    #[cfg_attr(flux, flux::spec(fn(GuestRange[@range], GuestRange[@other]) -> bool[range.start <= other.last && other.start <= range.last]))]
    pub const fn overlaps(self, other: GuestRange) -> bool {
        self.start <= other.last && other.start <= self.last
    }
}

/// Flux が誤りを見つけることを確かめるための、わざと誤った定義。`scripts/ci/flux.sh` だけが有効にする。
#[cfg(flux_broken)]
#[cfg_attr(flux, flux::spec(fn(start: GuestAddr, len: u64) -> Option<GuestRange>[true]))]
const fn flux_broken_range(start: GuestAddr, len: u64) -> Option<GuestRange> {
    GuestRange::new(start, len)
}

/// ゲスト仮想アドレスの幅。Wasm のメモリアドレス幅とは別に決まる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddrWidth {
    /// 32 ビット。
    Bits32,
    /// 64 ビット。
    Bits64,
}

impl AddrWidth {
    /// 表現できる最後のアドレス。
    #[must_use]
    pub const fn last(self) -> GuestAddr {
        match self {
            AddrWidth::Bits32 => GuestAddr(0xFFFF_FFFF),
            AddrWidth::Bits64 => GuestAddr(u64::MAX),
        }
    }
}

/// マッピングの粒度 (2 の冪)。ホストの確保粒度ではなく、ゲスト OS が見せる粒度を
/// Personality が選ぶ。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageSize(u64);

impl PageSize {
    /// `bytes` が 2 の冪なら作る。
    #[must_use]
    pub const fn new(bytes: u64) -> Option<PageSize> {
        if bytes.is_power_of_two() { Some(PageSize(bytes)) } else { None }
    }

    /// バイト数。
    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.0
    }

    /// `value` が粒度の倍数か。
    #[must_use]
    pub const fn divides(self, value: u64) -> bool {
        value & self.0.wrapping_sub(1) == 0
    }
}

/// メモリへのアクセスの種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// 読み出し。
    Read,
    /// 書き込み。
    Write,
    /// 命令としての実行。
    Execute,
}

/// マッピングの権限。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prot {
    read: bool,
    write: bool,
    execute: bool,
}

impl Prot {
    /// どのアクセスも許さない。
    pub const NONE: Prot = Prot::new(false, false, false);
    /// 読み出しだけ。
    pub const READ: Prot = Prot::new(true, false, false);
    /// 読み書き。
    pub const READ_WRITE: Prot = Prot::new(true, true, false);
    /// 読み出しと実行。
    pub const READ_EXECUTE: Prot = Prot::new(true, false, true);

    /// 個々の権限から作る。
    #[must_use]
    pub const fn new(read: bool, write: bool, execute: bool) -> Prot {
        Prot { read, write, execute }
    }

    /// `other` が許すアクセスを全て許すか (`other` からこの権限への変更で、権限が減らないか)。
    #[must_use]
    pub const fn covers(self, other: Prot) -> bool {
        (self.read || !other.read) && (self.write || !other.write) && (self.execute || !other.execute)
    }

    /// `access` を許すか。
    #[must_use]
    pub const fn allows(self, access: Access) -> bool {
        match access {
            Access::Read => self.read,
            Access::Write => self.write,
            Access::Execute => self.execute,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_refuses_empty_and_wrapping() {
        assert_eq!(GuestRange::new(GuestAddr::new(0), 0), None);
        assert_eq!(GuestRange::new(GuestAddr::new(u64::MAX), 2), None);
        let top = GuestRange::new(GuestAddr::new(u64::MAX), 1).unwrap();
        assert_eq!(top.last(), GuestAddr::new(u64::MAX));
        assert_eq!(top.byte_len(), Some(1));
        let all = GuestRange::spanning(GuestAddr::new(0), GuestAddr::new(u64::MAX)).unwrap();
        assert_eq!(all.byte_len(), None);
        assert_eq!(GuestRange::spanning(GuestAddr::new(2), GuestAddr::new(1)), None);
        // 一バイトの範囲は作れる。
        let one = GuestRange::spanning(GuestAddr::new(2), GuestAddr::new(2)).unwrap();
        assert_eq!(one.byte_len(), Some(1));
    }

    #[test]
    fn overlap_is_inclusive_of_both_ends() {
        let a = GuestRange::new(GuestAddr::new(0x1000), 0x1000).unwrap();
        let touching = GuestRange::new(GuestAddr::new(0x2000), 0x1000).unwrap();
        let inside = GuestRange::new(GuestAddr::new(0x1FFF), 1).unwrap();
        assert!(!a.overlaps(touching));
        assert!(a.overlaps(inside) && inside.overlaps(a));
        assert!(a.contains(GuestAddr::new(0x1FFF)) && !a.contains(GuestAddr::new(0x2000)));
    }

    #[test]
    fn a_protection_covers_exactly_its_subsets() {
        let all = [false, true];
        for (r, w, x) in all.into_iter().flat_map(|r| all.into_iter().flat_map(move |w| all.map(|x| (r, w, x)))) {
            for (r2, w2, x2) in all.into_iter().flat_map(|r| all.into_iter().flat_map(move |w| all.map(|x| (r, w, x)))) {
                let expected = (r || !r2) && (w || !w2) && (x || !x2);
                let subset = [(r, r2), (w, w2), (x, x2)].iter().all(|(have, want)| *have >= *want);
                assert_eq!(expected, subset);
                assert_eq!(Prot::new(r, w, x).covers(Prot::new(r2, w2, x2)), subset, "{r}{w}{x} / {r2}{w2}{x2}");
            }
        }
    }

    #[test]
    fn page_size_is_a_power_of_two() {
        assert_eq!(PageSize::new(0), None);
        assert_eq!(PageSize::new(3000), None);
        let page = PageSize::new(4096).unwrap();
        assert!(page.divides(0x3000) && !page.divides(0x3001));
        assert!(PageSize::new(1).unwrap().divides(7));
    }

    #[test]
    fn prot_answers_each_access() {
        assert!(!Prot::NONE.allows(Access::Read));
        assert!(Prot::READ_WRITE.allows(Access::Write) && !Prot::READ_WRITE.allows(Access::Execute));
        assert!(Prot::READ_EXECUTE.allows(Access::Execute) && !Prot::READ_EXECUTE.allows(Access::Write));
    }
}
