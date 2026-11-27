//! 型付き ID と、世代を持つ表。
//!
//! 内部 ID はスロット番号と世代番号を持つ。種類は型引数で区別するので、プロセスの ID を
//! スレッドの ID として渡すとコンパイルできない。スロットを再利用するたびに世代を進め、
//! 古い ID が新しい資源を指さないようにする (台帳 ID-1)。世代が表現の上限に達した
//! スロットは退役させ、二度と再利用しない。
//!
//! ID は発行元の実行インスタンスも持つ。別の `Kernel` が発行した ID は、スロットと世代が
//! たまたま一致しても資源を指さず、[`Error::Foreign`] になる。写しから復元した状態は、
//! 復元元が写しの時点までに発行した ID を引き継ぐ (資源を発行元ごと写すため)。復元の後に
//! 復元元が発行した ID は、復元先の資源を指さない。
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
use crate::kernel::InstanceId;
use crate::store::{List, TryClone};

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
    /// 通信路。
    Channel,
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
        /// 通信路。
        Channel => Channels;
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
/// 通信路の ID。
pub type ChannelId = Id<kind::Channel>;

/// 種類 `K` の資源を指す ID。
pub struct Id<K: Kind> {
    slot: u32,
    generation: NonZeroU32,
    issuer: InstanceId,
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

    /// この ID を発行した実行インスタンス。
    #[must_use]
    pub const fn issuer(self) -> InstanceId {
        self.issuer
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
        self.slot == other.slot && self.generation == other.generation && self.issuer == other.issuer
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
        (self.slot, self.generation, self.issuer.get()).cmp(&(other.slot, other.generation, other.issuer.get()))
    }
}
impl<K: Kind> fmt::Debug for Id<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}#{}.{}@{}", K::TAG, self.slot, self.generation, self.issuer.get())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Entry<T> {
    Live(T),
    Free,
    /// 世代を使い切った。二度と使わない。
    Retired,
}

#[derive(Debug, PartialEq, Eq)]
struct Slot<T> {
    generation: NonZeroU32,
    /// いまの世代の資源を発行した実行インスタンス。
    issuer: InstanceId,
    cell: Entry<T>,
}

impl<T: TryClone> TryClone for Slot<T> {
    fn try_clone(&self) -> Result<Slot<T>, Error> {
        let cell = match &self.cell {
            Entry::Live(value) => Entry::Live(value.try_clone()?),
            Entry::Free => Entry::Free,
            Entry::Retired => Entry::Retired,
        };
        Ok(Slot { generation: self.generation, issuer: self.issuer, cell })
    }
}

impl<K: Kind> TryClone for Id<K> {
    fn try_clone(&self) -> Result<Id<K>, Error> {
        Ok(*self)
    }
}

impl<K: Kind, T: TryClone> TryClone for Arena<K, T> {
    fn try_clone(&self) -> Result<Arena<K, T>, Error> {
        let Arena { slots, free, live, limit, first_generation, issuer, kind } = self;
        Ok(Arena {
            slots: slots.try_clone()?,
            free: free.try_clone()?,
            issuer: *issuer,
            live: *live,
            limit: *limit,
            first_generation: *first_generation,
            kind: *kind,
        })
    }
}

/// 実行インスタンスと、復元元をさかのぼった祖先の列。ID の発行元がこの系列にあるかを判定する。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Lineage {
    instance: InstanceId,
    /// 復元元の実行インスタンス。古い順。
    ancestors: List<InstanceId>,
}

impl Lineage {
    pub(crate) const fn new(instance: InstanceId) -> Lineage {
        Lineage { instance, ancestors: List::new() }
    }

    pub(crate) const fn instance(&self) -> InstanceId {
        self.instance
    }

    /// この系列を引き継ぐ、実行インスタンス `instance` の系列。
    pub(crate) fn succeeded_by(&self, instance: InstanceId) -> Result<Lineage, Error> {
        let mut ancestors = List::new();
        ancestors.reserve_total(self.ancestors.len().saturating_add(1))?;
        for ancestor in self.ancestors.iter() {
            ancestors.push(*ancestor)?;
        }
        ancestors.push(self.instance)?;
        Ok(Lineage { instance, ancestors })
    }

    /// 同じ系列の写し。
    pub(crate) fn same(&self) -> Result<Lineage, Error> {
        Ok(Lineage { instance: self.instance, ancestors: self.ancestors.try_clone()? })
    }

    /// `issuer` が、この実行インスタンスか、その祖先か。
    pub(crate) fn includes(&self, issuer: InstanceId) -> bool {
        issuer == self.instance || self.ancestors.iter().any(|ancestor| *ancestor == issuer)
    }

    /// 表に無かった ID の失敗。発行元がこの系列なら、解放済みの古い ID である。
    /// そうでなければ、別の実行インスタンスの ID である。
    pub(crate) fn missing<K: Kind>(&self, id: Id<K>) -> Error {
        if self.includes(id.issuer) { Error::Stale(K::TAG) } else { Error::Foreign }
    }
}

/// 世代を持つ表。
#[derive(Debug)]
#[cfg_attr(flux, flux::refined_by(live: int, limit: int))]
#[cfg_attr(flux, flux::invariant(live <= limit))]
pub(crate) struct Arena<K: Kind, T> {
    slots: List<Slot<T>>,
    /// 再利用できるスロット。後入れ先出し。容量は常に `slots` の長さ以上を確保してある。
    free: List<u32>,
    #[cfg_attr(flux, flux::field(u32[live]))]
    live: u32,
    #[cfg_attr(flux, flux::field({u32[limit] | live <= limit}))]
    limit: u32,
    first_generation: NonZeroU32,
    /// これから作る資源の発行元。
    issuer: InstanceId,
    kind: PhantomData<fn() -> K>,
}

impl<K: Kind, T: PartialEq> PartialEq for Arena<K, T> {
    fn eq(&self, other: &Self) -> bool {
        self.slots == other.slots
            && self.free == other.free
            && self.live == other.live
            && self.limit == other.limit
            && self.issuer == other.issuer
    }
}
impl<K: Kind, T: Eq> Eq for Arena<K, T> {}

impl<K: Kind, T> Arena<K, T> {
    #[cfg_attr(flux, flux::spec(fn(issuer: InstanceId, limit: u32) -> Arena<K, T>[0, limit]))]
    pub(crate) const fn new(issuer: InstanceId, limit: u32) -> Arena<K, T> {
        Arena::starting_at(issuer, limit, NonZeroU32::MIN)
    }

    /// 新しいスロットの最初の世代を指定して作る。世代の使い切りを試験・証明するための入口。
    #[cfg_attr(flux, flux::spec(fn(issuer: InstanceId, limit: u32, first_generation: NonZeroU32) -> Arena<K, T>[0, limit]))]
    pub(crate) const fn starting_at(issuer: InstanceId, limit: u32, first_generation: NonZeroU32) -> Arena<K, T> {
        Arena { slots: List::new(), free: List::new(), live: 0, limit, first_generation, issuer, kind: PhantomData }
    }

    /// これから作る資源の発行元を変える。写しを別の実行インスタンスとして復元するときに使う。
    /// 既にある資源の発行元は変えない (それらの ID は復元の前に配られている)。
    pub(crate) const fn reissue(&mut self, issuer: InstanceId) {
        self.issuer = issuer;
    }

    #[cfg_attr(flux, flux::spec(fn(&Arena<K, T>[@arena]) -> u32[arena.live]))]
    pub(crate) fn len(&self) -> u32 {
        self.live
    }

    /// 次の [`Arena::insert`] が成功することを保証する。
    #[cfg_attr(flux, flux::spec(fn(self: &mut Arena<K, T>[@arena]) -> Result<(), Error>{ok: ok => arena.live < arena.limit}))]
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

    /// 続く `count` 回の [`Arena::insert`] が成功することを保証する。
    #[cfg_attr(flux, flux::spec(fn(self: &mut Arena<K, T>[@arena], count: u32) -> Result<(), Error>{ok: ok => arena.live + count <= arena.limit}))]
    pub(crate) fn reserve_many(&mut self, count: u32) -> Result<(), Error> {
        match self.live.checked_add(count) {
            Some(after) if after <= self.limit => {}
            Some(_) | None => return Err(Error::Exhausted(K::RESOURCE)),
        }
        let fresh = usize::try_from(count).unwrap_or(usize::MAX).saturating_sub(self.free.len());
        let total = self.slots.len().checked_add(fresh).filter(|total| u32::try_from(*total).is_ok());
        let total = total.ok_or(Error::Exhausted(Resource::Identifiers))?;
        self.slots.reserve(fresh)?;
        self.free.reserve_total(total)
    }

    #[cfg_attr(flux, flux::spec(fn(self: &strg Arena<K, T>[@arena], value: T) -> Result<Id<K>, Error>[#ok] ensures self: Arena<K, T>[if ok { arena.live + 1 } else { arena.live }, arena.limit]))]
    pub(crate) fn insert(&mut self, value: T) -> Result<Id<K>, Error> {
        self.reserve()?;
        let id = match self.free.pop() {
            Some(slot) => {
                let entry = usize::try_from(slot).ok().and_then(|at| self.slots.get_mut(at));
                let Some(entry) = entry else {
                    return Err(Error::Broken(crate::error::Invariant::IdFresh));
                };
                entry.cell = Entry::Live(value);
                entry.issuer = self.issuer;
                Id { slot, generation: entry.generation, issuer: self.issuer, kind: PhantomData }
            }
            None => {
                let slot =
                    u32::try_from(self.slots.len()).map_err(|_| Error::Exhausted(Resource::Identifiers))?;
                self.slots.push(Slot { generation: self.first_generation, issuer: self.issuer, cell: Entry::Live(value) })?;
                Id { slot, generation: self.first_generation, issuer: self.issuer, kind: PhantomData }
            }
        };
        self.live = self.live.saturating_add(1);
        Ok(id)
    }

    /// Flux が誤りを見つけることを確かめるための、わざと誤った定義。`scripts/ci/flux.sh` だけが有効にする。
    #[cfg(flux_broken)]
    #[cfg_attr(flux, flux::spec(fn(self: &strg Arena<K, T>[@arena], value: T) -> Result<Id<K>, Error> ensures self: Arena<K, T>[arena.live + 1, arena.limit]))]
    fn flux_broken_insert(&mut self, value: T) -> Result<Id<K>, Error> {
        self.insert(value)
    }

    fn slot(&self, id: Id<K>) -> Option<&Slot<T>> {
        let slot = self.slots.get(usize::try_from(id.slot).ok()?)?;
        (slot.generation == id.generation && slot.issuer == id.issuer).then_some(slot)
    }

    /// 公開の操作が受け取った ID を引く。無ければ、発行元がこの系列かどうかで
    /// [`Error::Stale`] と [`Error::Foreign`] に分ける。
    pub(crate) fn find<'a>(&'a self, id: Id<K>, lineage: &Lineage) -> Result<&'a T, Error> {
        self.get(id).ok_or_else(|| lineage.missing(id))
    }

    /// [`Arena::find`] の、書き換えられる版。
    pub(crate) fn find_mut<'a>(&'a mut self, id: Id<K>, lineage: &Lineage) -> Result<&'a mut T, Error> {
        self.get_mut(id).ok_or_else(|| lineage.missing(id))
    }

    /// 公開の操作が受け取った ID の資源を取り除く。
    pub(crate) fn take(&mut self, id: Id<K>, lineage: &Lineage) -> Result<T, Error> {
        self.remove(id).ok_or_else(|| lineage.missing(id))
    }

    pub(crate) fn get(&self, id: Id<K>) -> Option<&T> {
        match &self.slot(id)?.cell {
            Entry::Live(value) => Some(value),
            Entry::Free | Entry::Retired => None,
        }
    }

    pub(crate) fn get_mut(&mut self, id: Id<K>) -> Option<&mut T> {
        let slot = self.slots.get_mut(usize::try_from(id.slot).ok()?)?;
        if slot.generation != id.generation || slot.issuer != id.issuer {
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
    #[must_use = "取り除けなかったこと (None) を呼び出し側が扱う"]
    #[cfg_attr(flux, flux::spec(fn(self: &strg Arena<K, T>[@arena], id: Id<K>) -> Option<T>[#some] ensures self: Arena<K, T>{after: after.limit == arena.limit && after.live <= arena.live && arena.live <= after.live + 1 && (some && arena.live > 0 => after.live == arena.live - 1)}))]
    pub(crate) fn remove(&mut self, id: Id<K>) -> Option<T> {
        let slot = self.slots.get_mut(usize::try_from(id.slot).ok()?)?;
        if slot.generation != id.generation || slot.issuer != id.issuer || !matches!(slot.cell, Entry::Live(_)) {
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
            Entry::Live(value) => {
                Some((Id { slot, generation: entry.generation, issuer: entry.issuer, kind: PhantomData }, value))
            }
            Entry::Free | Entry::Retired => None,
        })
    }

    /// 取り除いたスロットの番号を戻すための容量があるか。[`Arena::remove`] は確保しない。
    pub(crate) fn reserved(&self) -> bool {
        self.free.capacity() >= self.slots.len()
    }

    /// 空きスロットの列の余った容量を捨てる。検査が違反を検出できることを確かめる試験のための入口。
    #[cfg(test)]
    pub(crate) fn corrupt_capacity(&mut self) {
        self.free.corrupt_capacity();
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
        let mut arena = Things::new(InstanceId::new(1), 4);
        let ids = [10, 20, 30].map(|value| arena.insert(value).unwrap());
        assert!(arena.remove(ids[1]).is_some());
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
            let mut arena = Things::new(InstanceId::new(1), limit);
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
        assert!(reused.remove(extra).is_some());
        assert!(reused != build(4, 1));
    }

    #[test]
    fn stale_id_resolves_nothing_after_reuse() {
        let mut arena = Things::new(InstanceId::new(1), 4);
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
        let mut arena = Things::new(InstanceId::new(1), 1);
        let id = arena.insert(1).unwrap();
        assert_eq!(arena.insert(2), Err(Error::Exhausted(Resource::Objects)));
        assert!(arena.remove(id).is_some());
        assert!(arena.insert(3).is_ok());
    }

    #[test]
    fn exhausted_generation_retires_the_slot() {
        let mut arena = Things::starting_at(InstanceId::new(1), 4, NonZeroU32::MAX);
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
        let mut arena = Things::new(InstanceId::new(1), 8);
        let a = arena.insert(1).unwrap();
        let b = arena.insert(2).unwrap();
        let c = arena.insert(3).unwrap();
        assert!(arena.remove(b).is_some());
        let seen: Vec<_> = arena.iter().map(|(id, v)| (id, *v)).collect();
        assert_eq!(seen, vec![(a, 1), (c, 3)]);
    }

    #[test]
    fn debug_names_the_kind() {
        let mut arena = Things::new(InstanceId::new(1), 1);
        let id = arena.insert(1).unwrap();
        assert_eq!(format!("{id:?}"), "Object#0.1@1");
    }
}
