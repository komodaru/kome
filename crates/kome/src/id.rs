//! 型付き ID と、世代を持つ表。
//!
//! 内部 ID はスロット番号と世代番号を持つ。種類は型引数で区別するので、プロセスの ID を
//! スレッドの ID として渡すとコンパイルできない。スロットを再利用するたびに世代を進め、
//! 古い ID が新しい資源を指さないようにする (台帳 ID-1)。世代が表現の上限に達した
//! スロットは退役させ、二度と再利用しない。
//!
//! ゲストの HANDLE や fd の数値は Personality の表が内部 ID へ対応づける。ゲスト OS が
//! 許す数値の再利用と、ここでの世代管理は別の契約である。
//!
//! 種類の違う ID は代入できない。下の二つは型名だけが違う。
//!
//! ```
//! fn f(process: kome::ProcessId) { let _same: kome::ProcessId = process; }
//! ```
//!
//! ```compile_fail
//! fn f(process: kome::ProcessId) { let _thread: kome::ThreadId = process; }
//! ```
//!
//! ```compile_fail
//! // ID を数値から作る入口は無い。
//! let _ = kome::ThreadId { slot: 0, generation: 1 };
//! ```

use core::fmt;
use core::marker::PhantomData;
use core::num::NonZeroU32;

use crate::error::{Error, Resource};
use crate::store::List;

mod sealed {
    pub trait Sealed {}
}

/// ID の種類。kome の外では実装できない。
pub trait Kind: sealed::Sealed {
    /// 種類の名前。
    const TAG: Tag;
    /// この種類の表が満杯のときに報告する資源。
    const RESOURCE: Resource;
}

/// ID の種類の名前。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tag {
    /// プロセス。
    Process,
    /// スレッド。
    Thread,
    /// 待機可能な資源。
    Object,
    /// メモリの実体。
    Backing,
    /// タイマ。
    Timer,
    /// I/O 要求。
    Io,
}

/// ID の種類を表す型。値は持たない。
pub mod kind {
    use super::{Kind, Resource, Tag, sealed};

    macro_rules! kinds {
        ($($(#[$doc:meta])* $name:ident => $resource:ident;)*) => {$(
            $(#[$doc])*
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            pub enum $name {}
            impl sealed::Sealed for $name {}
            impl Kind for $name {
                const TAG: Tag = Tag::$name;
                const RESOURCE: Resource = Resource::$resource;
            }
        )*};
    }

    kinds! {
        /// プロセス。
        Process => Processes;
        /// スレッド。
        Thread => Threads;
        /// 待機可能な資源。
        Object => Objects;
        /// メモリの実体。
        Backing => Backings;
        /// タイマ。
        Timer => Timers;
        /// I/O 要求。
        Io => Ios;
    }
}

/// プロセスの ID。
pub type ProcessId = Id<kind::Process>;
/// スレッドの ID。
pub type ThreadId = Id<kind::Thread>;
/// 待機可能な資源の ID。
pub type ObjectId = Id<kind::Object>;
/// メモリの実体の ID。
pub type BackingId = Id<kind::Backing>;
/// タイマの ID。
pub type TimerId = Id<kind::Timer>;
/// I/O 要求の ID。
pub type IoId = Id<kind::Io>;

/// 種類 `K` の資源を指す ID。
pub struct Id<K: Kind> {
    slot: u32,
    generation: NonZeroU32,
    kind: PhantomData<fn() -> K>,
}

impl<K: Kind> Id<K> {
    /// スロット番号。記録と診断のための値で、ここから ID は作れない。
    #[must_use]
    pub const fn slot(self) -> u32 {
        self.slot
    }

    /// 世代番号。記録と診断のための値。
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.generation.get()
    }
}

impl<K: Kind> Clone for Id<K> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<K: Kind> Copy for Id<K> {}
impl<K: Kind> PartialEq for Id<K> {
    fn eq(&self, other: &Self) -> bool {
        self.slot == other.slot && self.generation == other.generation
    }
}
impl<K: Kind> Eq for Id<K> {}
impl<K: Kind> PartialOrd for Id<K> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<K: Kind> Ord for Id<K> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        (self.slot, self.generation).cmp(&(other.slot, other.generation))
    }
}
impl<K: Kind> fmt::Debug for Id<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}#{}.{}", K::TAG, self.slot, self.generation)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Entry<T> {
    Live(T),
    Free,
    /// 世代を使い切った。二度と使わない。
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Slot<T> {
    generation: NonZeroU32,
    cell: Entry<T>,
}

/// 世代を持つ表。
#[derive(Clone, Debug)]
pub(crate) struct Arena<K: Kind, T> {
    slots: List<Slot<T>>,
    /// 再利用できるスロット。後入れ先出し。容量は常に `slots` の長さ以上を確保してある。
    free: List<u32>,
    live: u32,
    limit: u32,
    first_generation: NonZeroU32,
    kind: PhantomData<fn() -> K>,
}

impl<K: Kind, T: PartialEq> PartialEq for Arena<K, T> {
    fn eq(&self, other: &Self) -> bool {
        self.slots == other.slots && self.free == other.free && self.live == other.live && self.limit == other.limit
    }
}
impl<K: Kind, T: Eq> Eq for Arena<K, T> {}

impl<K: Kind, T> Arena<K, T> {
    pub(crate) const fn new(limit: u32) -> Arena<K, T> {
        Arena::starting_at(limit, NonZeroU32::MIN)
    }

    /// 新しいスロットの最初の世代を指定して作る。世代の使い切りを試験・証明するための入口。
    pub(crate) const fn starting_at(limit: u32, first_generation: NonZeroU32) -> Arena<K, T> {
        Arena { slots: List::new(), free: List::new(), live: 0, limit, first_generation, kind: PhantomData }
    }

    pub(crate) fn len(&self) -> u32 {
        self.live
    }

    /// 次の [`Arena::insert`] が成功することを保証する。
    pub(crate) fn reserve(&mut self) -> Result<(), Error> {
        if self.live >= self.limit {
            return Err(Error::Exhausted(K::RESOURCE));
        }
        if self.free.is_empty() {
            if u32::try_from(self.slots.len()).is_err() {
                return Err(Error::Exhausted(Resource::Identifiers));
            }
            self.slots.reserve(1)?;
            self.free.reserve_total(self.slots.len().saturating_add(1))?;
        }
        Ok(())
    }

    pub(crate) fn insert(&mut self, value: T) -> Result<Id<K>, Error> {
        self.reserve()?;
        let id = match self.free.pop() {
            Some(slot) => {
                let entry = usize::try_from(slot).ok().and_then(|at| self.slots.get_mut(at));
                let Some(entry) = entry else {
                    return Err(Error::Broken(crate::error::Invariant::IdFresh));
                };
                entry.cell = Entry::Live(value);
                Id { slot, generation: entry.generation, kind: PhantomData }
            }
            None => {
                let slot =
                    u32::try_from(self.slots.len()).map_err(|_| Error::Exhausted(Resource::Identifiers))?;
                self.slots.push(Slot { generation: self.first_generation, cell: Entry::Live(value) })?;
                Id { slot, generation: self.first_generation, kind: PhantomData }
            }
        };
        self.live = self.live.saturating_add(1);
        Ok(id)
    }

    fn slot(&self, id: Id<K>) -> Option<&Slot<T>> {
        let slot = self.slots.get(usize::try_from(id.slot).ok()?)?;
        (slot.generation == id.generation).then_some(slot)
    }

    pub(crate) fn get(&self, id: Id<K>) -> Option<&T> {
        match &self.slot(id)?.cell {
            Entry::Live(value) => Some(value),
            Entry::Free | Entry::Retired => None,
        }
    }

    pub(crate) fn get_mut(&mut self, id: Id<K>) -> Option<&mut T> {
        let slot = self.slots.get_mut(usize::try_from(id.slot).ok()?)?;
        if slot.generation != id.generation {
            return None;
        }
        match &mut slot.cell {
            Entry::Live(value) => Some(value),
            Entry::Free | Entry::Retired => None,
        }
    }

    pub(crate) fn contains(&self, id: Id<K>) -> bool {
        self.get(id).is_some()
    }

    /// 資源を取り除く。スロットの世代を進め、進められなければ退役させる。確保しない。
    pub(crate) fn remove(&mut self, id: Id<K>) -> Option<T> {
        let slot = self.slots.get_mut(usize::try_from(id.slot).ok()?)?;
        if slot.generation != id.generation || !matches!(slot.cell, Entry::Live(_)) {
            return None;
        }
        let taken = core::mem::replace(&mut slot.cell, Entry::Retired);
        if let Some(next) = slot.generation.checked_add(1) {
            // 容量は insert が確保してある。万一無ければ退役のままにする (再利用しないだけで安全)。
            if self.free.push_within(id.slot).is_ok() {
                slot.generation = next;
                slot.cell = Entry::Free;
            }
        }
        self.live = self.live.saturating_sub(1);
        match taken {
            Entry::Live(value) => Some(value),
            Entry::Free | Entry::Retired => None,
        }
    }

    /// 生きている資源をスロット順に返す。
    pub(crate) fn iter(&self) -> impl Iterator<Item = (Id<K>, &T)> {
        self.iter_from(0)
    }

    /// スロット番号が `first` 以上の生きている資源をスロット順に返す。
    pub(crate) fn iter_from(&self, first: u32) -> impl Iterator<Item = (Id<K>, &T)> {
        let skipped = usize::try_from(first).unwrap_or(usize::MAX);
        (first..=u32::MAX).zip(self.slots.iter().skip(skipped)).filter_map(|(slot, entry)| match &entry.cell {
            Entry::Live(value) => Some((Id { slot, generation: entry.generation, kind: PhantomData }, value)),
            Entry::Free | Entry::Retired => None,
        })
    }

    /// 生存数を書き換える。検査が違反を検出できることを確かめる試験のための入口。
    #[cfg(test)]
    pub(crate) fn corrupt_live(&mut self, live: u32) {
        self.live = live;
    }

    /// 表の内部が一貫しているか。空きスロットの列と生存数を数え直す。
    pub(crate) fn consistent(&self) -> bool {
        let mut live = 0u32;
        let mut free = 0usize;
        for entry in self.slots.iter() {
            match entry.cell {
                Entry::Live(_) => live = live.saturating_add(1),
                Entry::Free => free = free.saturating_add(1),
                Entry::Retired => {}
            }
        }
        let listed_free = self.free.iter().all(|slot| {
            usize::try_from(*slot)
                .ok()
                .and_then(|at| self.slots.get(at))
                .is_some_and(|entry| matches!(entry.cell, Entry::Free))
        });
        let unique = self.free.iter().enumerate().all(|(at, slot)| {
            self.free.as_slice().get(..at).is_some_and(|before| !before.contains(slot))
        });
        live == self.live && free == self.free.len() && listed_free && unique && live <= self.limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Things = Arena<kind::Object, u32>;

    #[test]
    fn iteration_can_start_at_a_slot() {
        let mut arena = Things::new(4);
        let ids = [10, 20, 30].map(|value| arena.insert(value).unwrap());
        arena.remove(ids[1]);
        let from = |first: u32| arena.iter_from(first).map(|(id, value)| (id.slot(), *value)).collect::<Vec<_>>();
        assert_eq!(from(0), [(0, 10), (2, 30)]);
        assert_eq!(from(1), [(2, 30)]);
        assert_eq!(from(2), [(2, 30)]);
        assert_eq!(from(3), []);
        assert_eq!(from(u32::MAX), []);
    }

    #[test]
    fn arenas_are_equal_only_when_everything_matches() {
        let build = |limit: u32, value: u32| {
            let mut arena = Things::new(limit);
            arena.insert(value).unwrap();
            arena
        };
        assert!(build(4, 1) == build(4, 1));
        // 中身だけが違う。
        assert!(build(4, 1) != build(4, 2));
        // 上限だけが違う。
        assert!(build(4, 1) != build(5, 1));
        // 空きスロットの有無だけが違う (生存数と上限は同じ)。
        let mut reused = build(4, 1);
        let extra = reused.insert(9).unwrap();
        reused.remove(extra);
        assert!(reused != build(4, 1));
    }

    #[test]
    fn stale_id_resolves_nothing_after_reuse() {
        let mut arena = Things::new(4);
        let old = arena.insert(10).unwrap();
        assert_eq!(arena.remove(old), Some(10));
        let new = arena.insert(20).unwrap();
        assert_eq!(new.slot(), old.slot());
        assert_ne!(new, old);
        assert_eq!(arena.get(old), None);
        assert_eq!(arena.remove(old), None);
        assert_eq!(arena.get(new), Some(&20));
        assert!(arena.consistent());
    }

    #[test]
    fn limit_is_a_refusal_not_a_panic() {
        let mut arena = Things::new(1);
        let id = arena.insert(1).unwrap();
        assert_eq!(arena.insert(2), Err(Error::Exhausted(Resource::Objects)));
        arena.remove(id);
        assert!(arena.insert(3).is_ok());
    }

    #[test]
    fn exhausted_generation_retires_the_slot() {
        let mut arena = Things::starting_at(4, NonZeroU32::MAX);
        let last = arena.insert(1).unwrap();
        assert_eq!(last.generation(), u32::MAX);
        assert_eq!(arena.remove(last), Some(1));
        let next = arena.insert(2).unwrap();
        assert_ne!(next.slot(), last.slot(), "世代を使い切ったスロットを再利用した");
        assert_eq!(arena.get(last), None);
        assert!(arena.consistent());
    }

    #[test]
    fn iter_is_in_slot_order() {
        let mut arena = Things::new(8);
        let a = arena.insert(1).unwrap();
        let b = arena.insert(2).unwrap();
        let c = arena.insert(3).unwrap();
        arena.remove(b);
        let seen: Vec<_> = arena.iter().map(|(id, v)| (id, *v)).collect();
        assert_eq!(seen, vec![(a, 1), (c, 3)]);
    }

    #[test]
    fn debug_names_the_kind() {
        let mut arena = Things::new(1);
        let id = arena.insert(1).unwrap();
        assert_eq!(format!("{id:?}"), "Object#0.1");
    }
}
