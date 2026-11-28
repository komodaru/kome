//! 失敗を返せる確保だけを通す入れ物。
//!
//! kome の状態は `alloc` の `Vec`・`VecDeque` を直接使わず、この二つを通す。
//! 確保はすべて `try_reserve` で行い、失敗は `Exhausted(HostMemory)` になる。
//! 取り除く操作は確保しない。kome-lint の `raw-collections` がこのファイル以外での
//! 直接利用を拒否する。

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::error::{Error, Invariant, Resource};

/// 確保の失敗を返す複製。確保済みの容量も写す (写しでも、解放・終了・起床の経路は確保しない)。
///
/// 入れ物は `Clone` を実装しない。入れ物を持つ型は、この trait でだけ複製できる。
pub(crate) trait TryClone: Sized {
    fn try_clone(&self) -> Result<Self, Error>;
}

/// 入れ物を持たない型の複製は、値の写しである。
macro_rules! plain_try_clone {
    ($($type:ty),* $(,)?) => {$(
        impl $crate::store::TryClone for $type {
            fn try_clone(&self) -> Result<Self, $crate::error::Error> {
                Ok(*self)
            }
        }
    )*};
}
pub(crate) use plain_try_clone;

plain_try_clone!(u32, crate::kernel::InstanceId);

/// 順序を持つ列。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct List<T> {
    items: Vec<T>,
}

impl<T> List<T> {
    pub(crate) const fn new() -> List<T> {
        List { items: Vec::new() }
    }

    /// `count` 個の `value` を並べた列。この後に要素を足さなければ、列の番地は変わらない。
    pub(crate) fn filled(value: T, count: usize) -> Result<List<T>, Error>
    where
        T: Clone,
    {
        let mut list = List::new();
        list.reserve_total(count)?;
        list.items.resize(count, value);
        Ok(list)
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

    /// 確保済みの容量。検査が、予約した容量が残っていることを確かめるのに使う。
    pub(crate) fn capacity(&self) -> usize {
        self.items.capacity()
    }

    /// 余っている容量を捨てる。検査が容量の不足を検出できることを確かめる試験のための入口。
    #[cfg(test)]
    pub(crate) fn corrupt_capacity(&mut self) {
        self.items.shrink_to_fit();
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

    /// 二つの位置の要素を入れ替える。どちらかが範囲の外なら何もせず `None`。
    #[must_use = "入れ替えられなかったこと (None) を呼び出し側が扱う"]
    pub(crate) fn swap(&mut self, a: usize, b: usize) -> Option<()> {
        if a < self.items.len() && b < self.items.len() {
            self.items.swap(a, b);
            Some(())
        } else {
            None
        }
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

impl<T: TryClone> TryClone for List<T> {
    fn try_clone(&self) -> Result<List<T>, Error> {
        let mut copy = List::new();
        copy.reserve(self.items.capacity())?;
        for item in &self.items {
            copy.push_within(item.try_clone()?).map_err(|_| Error::Exhausted(Resource::HostMemory))?;
        }
        Ok(copy)
    }
}

/// 先入れ先出しの列。
#[derive(Debug, PartialEq, Eq)]
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

    /// 確保済みの容量。検査が、予約した容量が残っていることを確かめるのに使う。
    pub(crate) fn capacity(&self) -> usize {
        self.items.capacity()
    }

    /// 余っている容量を捨てる。検査が容量の不足を検出できることを確かめる試験のための入口。
    #[cfg(test)]
    pub(crate) fn corrupt_capacity(&mut self) {
        self.items.shrink_to_fit();
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

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 全て取り除く。確保しない。容量は残る。
    pub(crate) fn clear(&mut self) {
        self.items.clear();
    }

    pub(crate) fn pop_front(&mut self) -> Option<T> {
        self.items.pop_front()
    }

    pub(crate) fn get(&self, at: usize) -> Option<&T> {
        self.items.get(at)
    }

    pub(crate) fn iter(&self) -> alloc::collections::vec_deque::Iter<'_, T> {
        self.items.iter()
    }

}

impl<T: Copy> Queue<T> {
    /// 確保済みの容量の中でだけ、末尾へ並べて追加する。入り切らなければ何も足さない。
    pub(crate) fn extend_within(&mut self, values: &[T]) -> Result<(), ()> {
        let after = self.items.len().checked_add(values.len()).ok_or(())?;
        if after > self.items.capacity() {
            return Err(());
        }
        self.items.extend(values.iter().copied());
        Ok(())
    }

    /// 先頭から `out` に入るぶんを写して取り除く。写した数を返す。確保しない。
    pub(crate) fn drain_into(&mut self, out: &mut [T]) -> usize {
        let mut moved = 0usize;
        for (slot, value) in out.iter_mut().zip(self.items.iter()) {
            *slot = *value;
            moved = moved.saturating_add(1);
        }
        self.items.drain(..moved);
        moved
    }
}

impl<T: TryClone> TryClone for Queue<T> {
    fn try_clone(&self) -> Result<Queue<T>, Error> {
        let mut copy = Queue::new();
        copy.reserve_total(self.items.capacity())?;
        for item in &self.items {
            copy.push_back_within(item.try_clone()?).map_err(|_| Error::Exhausted(Resource::HostMemory))?;
        }
        Ok(copy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_swap_outside_the_list_and_an_extension_past_the_capacity_change_nothing() {
        let mut list = List::new();
        list.push(1).unwrap();
        list.push(2).unwrap();
        assert_eq!(list.swap(0, 1), Some(()));
        assert_eq!(list.swap(0, 2), None);
        assert_eq!(list.swap(2, 0), None);
        assert_eq!(list.as_slice(), &[2, 1]);
        let mut queue = Queue::new();
        queue.reserve_total(2).unwrap();
        let room = queue.items.capacity();
        let too_many = vec![7u8; room.saturating_add(1)];
        assert_eq!(queue.extend_within(&too_many), Err(()));
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.extend_within(too_many.get(..room).unwrap()), Ok(()));
        assert_eq!(queue.len(), room);
    }

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
