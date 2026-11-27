//! アドレス空間の独立したモデル: ページ番号から中身への単純な表。
//!
//! kome のマッピング (区間の列、分割、世代) とは別の表現で同じ意味を持つ。
//! 両者の見え方が一致することを [`crate::sim`] が操作のたびに確かめる。
//!
//! 権限とアクセスの種類も、kome の型 (`Prot`、`Access`) を使わずにここで定義する。
//! モデルが kome の判定を呼ぶと、判定の誤りが両方に同じように現れて見つからない。
//! kome から借りるのは実体の ID だけである (lint `independent-model`)。

use std::collections::BTreeMap;

use kome::BackingId;

/// モデルの権限。入力の三つの真偽値をそのまま持つ。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perm {
    /// 読み出せる。
    pub read: bool,
    /// 書き込める。
    pub write: bool,
    /// 実行できる。
    pub execute: bool,
}

impl Perm {
    /// どのアクセスもできない。
    pub const NONE: Perm = Perm { read: false, write: false, execute: false };

    /// 3 ビットの数から作る (読み出し 1、書き込み 2、実行 4)。全部で 8 通り。
    #[must_use]
    pub const fn from_bits(bits: u8) -> Perm {
        Perm { read: bits & 1 != 0, write: bits & 2 != 0, execute: bits & 4 != 0 }
    }

    /// このアクセスができるか。
    #[must_use]
    pub const fn permits(self, touch: Touch) -> bool {
        match touch {
            Touch::Read => self.read,
            Touch::Write => self.write,
            Touch::Execute => self.execute,
        }
    }

    /// `to` へ変えると、できなくなるアクセスがあるか。
    #[must_use]
    pub fn loses(self, to: Perm) -> bool {
        Touch::ALL.into_iter().any(|touch| self.permits(touch) && !to.permits(touch))
    }
}

/// モデルのアクセスの種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Touch {
    /// 読み出し。
    Read,
    /// 書き込み。
    Write,
    /// 実行。
    Execute,
}

impl Touch {
    /// すべての種類。
    pub const ALL: [Touch; 3] = [Touch::Read, Touch::Write, Touch::Execute];
}

/// 一ページの中身。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    /// 予約だけ。
    Reserved,
    /// 実体の `offset` バイト目から始まるページ。
    Backed {
        /// 内容を持つ実体。
        backing: BackingId,
        /// 実体の中の位置。
        offset: u64,
        /// 権限。
        prot: Perm,
    },
}

/// モデルが予想する、一バイトのアクセスの結果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    /// マッピングが無い。
    Unmapped,
    /// 予約だけ。
    Reserved,
    /// 権限が無い。
    Denied,
    /// 実体のこの位置に届く。
    Reaches(BackingId, u64),
}

/// 一つのアドレス空間のモデル。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pages {
    pages: BTreeMap<u64, Page>,
}

impl Pages {
    /// 範囲のどこかにページがあるか。
    #[must_use]
    pub fn any(&self, first: u64, count: u64) -> bool {
        self.pages.range(first..first.saturating_add(count)).next().is_some()
    }

    /// 範囲のすべてが実体のあるページか。
    #[must_use]
    pub fn all_backed(&self, first: u64, count: u64) -> bool {
        (first..first.saturating_add(count)).all(|page| matches!(self.pages.get(&page), Some(Page::Backed { .. })))
    }

    /// 範囲へ、`source` の先頭から順にページを置く。
    pub fn map(&mut self, first: u64, count: u64, source: Option<(BackingId, u64)>, prot: Perm, page_size: u64) {
        for index in 0..count {
            let page = match source {
                None => Page::Reserved,
                Some((backing, offset)) => {
                    Page::Backed { backing, offset: offset.saturating_add(index.saturating_mul(page_size)), prot }
                }
            };
            self.pages.insert(first.saturating_add(index), page);
        }
    }

    /// 範囲のページを外す。外した数と、実体のあるページを外したかを返す。
    pub fn unmap(&mut self, first: u64, count: u64) -> (u64, bool) {
        let mut removed = 0u64;
        let mut backed = false;
        for page in first..first.saturating_add(count) {
            match self.pages.remove(&page) {
                Some(Page::Backed { .. }) => {
                    removed = removed.saturating_add(1);
                    backed = true;
                }
                Some(Page::Reserved) => removed = removed.saturating_add(1),
                None => {}
            }
        }
        (removed, backed)
    }

    /// 範囲の実体のあるページの権限を変える。変わったページがあるかと、できなくなった
    /// アクセスがあるかを返す。
    pub fn protect(&mut self, first: u64, count: u64, to: Perm) -> (bool, bool) {
        let mut changed = false;
        let mut lost = false;
        for page in first..first.saturating_add(count) {
            if let Some(Page::Backed { prot, .. }) = self.pages.get_mut(&page) {
                changed = changed || *prot != to;
                lost = lost || prot.loses(to);
                *prot = to;
            }
        }
        (changed, lost)
    }

    /// ページ `page` が、一つ前のページと同じ権限で、同じ実体の続きを見せているか
    /// (二つを一つの区間として表せるか)。
    #[must_use]
    pub fn continues(&self, page: u64, page_size: u64) -> bool {
        let Some(before) = page.checked_sub(1) else { return false };
        match (self.pages.get(&before), self.pages.get(&page)) {
            (
                Some(Page::Backed { backing, offset, prot }),
                Some(Page::Backed { backing: next, offset: follows, prot: same }),
            ) => backing == next && prot == same && offset.checked_add(page_size) == Some(*follows),
            (Some(Page::Backed { .. } | Page::Reserved) | None, Some(Page::Backed { .. } | Page::Reserved) | None) => false,
        }
    }

    /// ページの先頭から `within` バイト目へのアクセスの結果。
    #[must_use]
    pub fn expect(&self, page: u64, within: u64, touch: Touch) -> Expect {
        match self.pages.get(&page) {
            None => Expect::Unmapped,
            Some(Page::Reserved) => Expect::Reserved,
            Some(Page::Backed { backing, offset, prot }) => {
                if prot.permits(touch) {
                    Expect::Reaches(*backing, offset.saturating_add(within))
                } else {
                    Expect::Denied
                }
            }
        }
    }

    /// この実体を見せているページがあるか。
    #[must_use]
    pub fn shows(&self, backing: BackingId) -> bool {
        self.pages.values().any(|page| matches!(page, Page::Backed { backing: shown, .. } if *shown == backing))
    }

    /// 見せている実体の一覧。
    pub fn backings(&self) -> impl Iterator<Item = BackingId> + '_ {
        self.pages.values().filter_map(|page| match page {
            Page::Backed { backing, .. } => Some(*backing),
            Page::Reserved => None,
        })
    }
}
