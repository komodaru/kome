//! アドレス空間の独立したモデル: ページ番号から中身への単純な表。
//!
//! kome のマッピング (区間の列、分割、世代) とは別の表現で同じ意味を持つ。
//! 両者の見え方が一致することを [`crate::sim`] が操作のたびに確かめる。

use std::collections::BTreeMap;

use kome::{Access, BackingId, Prot};

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
        prot: Prot,
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
    pub fn map(&mut self, first: u64, count: u64, source: Option<(BackingId, u64)>, prot: Prot, page_size: u64) {
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

    /// 範囲の実体のあるページの権限を変える。
    pub fn protect(&mut self, first: u64, count: u64, to: Prot) {
        for page in first..first.saturating_add(count) {
            if let Some(Page::Backed { prot, .. }) = self.pages.get_mut(&page) {
                *prot = to;
            }
        }
    }

    /// ページの先頭から `within` バイト目へのアクセスの結果。
    #[must_use]
    pub fn expect(&self, page: u64, within: u64, access: Access) -> Expect {
        match self.pages.get(&page) {
            None => Expect::Unmapped,
            Some(Page::Reserved) => Expect::Reserved,
            Some(Page::Backed { backing, offset, prot }) => {
                if prot.allows(access) {
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
