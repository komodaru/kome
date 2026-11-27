//! 失敗を返せる確保だけを通す入れ物。
//!
//! kome の状態は `alloc` の `Vec`・`VecDeque` を直接使わず、この二つを通す。
//! 確保はすべて `try_reserve` で行い、失敗は `Exhausted(HostMemory)` になる。
//! 取り除く操作は確保しない。kome-lint の `raw-collections` がこのファイル以外での
//! 直接利用を拒否する。

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::error::{Error, Invariant, Resource};

/// 順序を持つ列。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct List<T> {
    items: Vec<T>,
}

impl<T> List<T> {
    pub(crate) const fn new() -> List<T> {
        List { items: Vec::new() }
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// あと `extra` 個を確保なしで入れられるようにする。
    pub(crate) fn reserve(&mut self, extra: usize) -> Result<(), Error> {
        self.items.try_reserve(extra).map_err(|_| Error::Exhausted(Resource::HostMemory))
    }

    /// 合計 `total` 個を確保なしで持てるようにする。
    pub(crate) fn reserve_total(&mut self, total: usize) -> Result<(), Error> {
        self.reserve(total.saturating_sub(self.items.len()))
    }

    pub(crate) fn push(&mut self, value: T) -> Result<(), Error> {
        self.reserve(1)?;
        self.items.push(value);
        Ok(())
    }

    /// 確保済みの容量の中でだけ追加する。容量が無ければ値を返す。
    pub(crate) fn push_within(&mut self, value: T) -> Result<(), T> {
        if self.items.len() < self.items.capacity() {
            self.items.push(value);
            Ok(())
        } else {
            Err(value)
        }
    }

    /// `at` の位置へ挿入する。`at` が長さを超えるのは呼び出し側の不変条件 `owner` の違反。
    pub(crate) fn insert(&mut self, at: usize, value: T, owner: Invariant) -> Result<(), Error> {
        if at > self.items.len() {
            return Err(Error::Broken(owner));
        }
        self.reserve(1)?;
        self.items.insert(at, value);
        Ok(())
    }

    #[must_use = "取り除けなかったこと (None) を呼び出し側が扱う"]
    pub(crate) fn remove(&mut self, at: usize) -> Option<T> {
        if at < self.items.len() { Some(self.items.remove(at)) } else { None }
    }

    pub(crate) fn pop(&mut self) -> Option<T> {
        self.items.pop()
    }

    pub(crate) fn get(&self, at: usize) -> Option<&T> {
        self.items.get(at)
    }

    pub(crate) fn get_mut(&mut self, at: usize) -> Option<&mut T> {
        self.items.get_mut(at)
    }

    pub(crate) fn iter(&self) -> core::slice::Iter<'_, T> {
        self.items.iter()
    }

    pub(crate) fn iter_mut(&mut self) -> core::slice::IterMut<'_, T> {
        self.items.iter_mut()
    }

    pub(crate) fn as_slice(&self) -> &[T] {
        &self.items
    }

    pub(crate) fn position(&self, found: impl FnMut(&T) -> bool) -> Option<usize> {
        self.items.iter().position(found)
    }

    /// 条件に合う最初の要素を取り除く。
    #[must_use = "取り除けなかったこと (None) を呼び出し側が扱う"]
    pub(crate) fn remove_first(&mut self, found: impl FnMut(&T) -> bool) -> Option<T> {
        let at = self.position(found)?;
        self.remove(at)
    }
}

/// 先入れ先出しの列。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Queue<T> {
    items: VecDeque<T>,
}

impl<T> Queue<T> {
    pub(crate) const fn new() -> Queue<T> {
        Queue { items: VecDeque::new() }
    }

    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    /// 合計 `total` 個を確保なしで持てるようにする。
    pub(crate) fn reserve_total(&mut self, total: usize) -> Result<(), Error> {
        let extra = total.saturating_sub(self.items.len());
        self.items.try_reserve(extra).map_err(|_| Error::Exhausted(Resource::HostMemory))
    }

    /// 確保済みの容量の中でだけ末尾へ追加する。容量が無ければ値を返す。
    pub(crate) fn push_back_within(&mut self, value: T) -> Result<(), T> {
        if self.items.len() < self.items.capacity() {
            self.items.push_back(value);
            Ok(())
        } else {
            Err(value)
        }
    }

    pub(crate) fn pop_front(&mut self) -> Option<T> {
        self.items.pop_front()
    }

    pub(crate) fn get(&self, at: usize) -> Option<&T> {
        self.items.get(at)
    }

    #[must_use = "取り除けなかったこと (None) を呼び出し側が扱う"]
    pub(crate) fn remove(&mut self, at: usize) -> Option<T> {
        self.items.remove(at)
    }

    pub(crate) fn iter(&self) -> alloc::collections::vec_deque::Iter<'_, T> {
        self.items.iter()
    }

    pub(crate) fn position(&self, found: impl FnMut(&T) -> bool) -> Option<usize> {
        self.items.iter().position(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_keeps_order_and_refuses_bad_insert() {
        let mut list = List::new();
        list.push(1).unwrap();
        list.push(3).unwrap();
        list.insert(1, 2, Invariant::TimeOrder).unwrap();
        assert_eq!(list.as_slice(), &[1, 2, 3]);
        assert_eq!(list.insert(5, 9, Invariant::TimeOrder), Err(Error::Broken(Invariant::TimeOrder)));
        assert_eq!(list.remove(7), None);
        // 長さちょうどの位置にも要素は無い。
        assert_eq!(list.remove(3), None);
        assert_eq!(list.remove_first(|v| *v == 2), Some(2));
        assert_eq!(list.as_slice(), &[1, 3]);
    }

    #[test]
    fn push_within_does_not_grow() {
        let mut list: List<u8> = List::new();
        assert_eq!(list.push_within(1), Err(1));
        list.reserve_total(1).unwrap();
        assert_eq!(list.push_within(1), Ok(()));
        let mut queue: Queue<u8> = Queue::new();
        assert_eq!(queue.push_back_within(1), Err(1));
        queue.reserve_total(2).unwrap();
        assert_eq!(queue.push_back_within(1), Ok(()));
        assert_eq!(queue.pop_front(), Some(1));
    }
}
