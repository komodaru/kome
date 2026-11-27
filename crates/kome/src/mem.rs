//! アドレス空間と、メモリの実体 (backing) への参照。
//!
//! アドレス空間は、ゲストが見る仮想アドレスの範囲と、内容を保持する実体の対応を持つ。
//! kome が持つのは範囲・対応・権限・世代であり、内容のバイト列はホスト実装が持つ。
//! `VirtualAlloc` の reserve・commit や `mmap` のフラグの意味は Personality が解釈し、
//! ここでは「予約だけのマッピング」と「実体のあるマッピング」の操作に直して呼ぶ。
//!
//! # 権限の変更と実体の寿命 (台帳 MEM-3、MEM-1)
//!
//! 実体のあるマッピングを外す、または権限を変えるとき、別の worker が古い世代の
//! マッピングで実行中かもしれない。二つの条件を別々に守る。
//!
//! - **変更後の権限を守る**: 変更の時点で実行中だったそのプロセスのスレッドへ中断を
//!   要求し、それらが実行権を返し終えるまで、そのプロセスのどのスレッドにも新しい
//!   実行権を貸さない。変更を要求したスレッドが再開する (変更の完了をゲストが見る)
//!   のは、古いアクセスがすべて止まった後になる。
//! - **使用中の実体を解放しない**: 外したマッピングの実体への参照は、古い世代の実行が
//!   残る間は「保留」として持ち続ける。解放してよいと通知するのは、保持・マッピング・
//!   保留がすべて無くなったときである。
//!
//! 実行器は実行権の世代で検証したマッピングだけを使い、世代が変わったら引き直す。
//!
//! # 基準の経路
//!
//! [`Kernel::translate`] は、範囲外・権限違反・マッピングの境界をまたぐアクセスを
//! 明示的に扱う基準の経路である。高速経路は、同じ結果になることをこの経路と比べて確かめる。

use crate::addr::{Access, AddrWidth, BackingOffset, GuestAddr, GuestRange, PageSize, Prot};
use crate::effect::Effect;
use crate::error::{Error, Invariant, Refusal, Resource};
use crate::id::{BackingId, ProcessId};
use crate::kernel::Kernel;
use crate::process::Life;
use crate::store::{List, TryClone, plain_try_clone};
use crate::thread::Run;

/// アドレス空間の世代。マッピングが変わるたびに進む。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MapGen(u64);

impl MapGen {
    /// 記録と診断のための数値。
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// マッピングの中身。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapSource {
    /// 範囲を占有するだけで、実体が無い。どのアクセスも失敗する。
    Reserved,
    /// 実体の `offset` から先を見せる。同じ実体を複数の範囲へ見せてよい (別名)。
    Backing {
        /// 内容を持つ実体。
        backing: BackingId,
        /// 範囲の先頭に対応する実体の中の位置。
        offset: BackingOffset,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mapping {
    pub(crate) start: u64,
    /// 最後のバイトのアドレス。
    pub(crate) last: u64,
    pub(crate) prot: Prot,
    pub(crate) source: MapSource,
}

impl Mapping {
    /// `from` (このマッピングの中のアドレス) から後ろだけを残したもの。
    fn tail_from(self, from: u64) -> Option<Mapping> {
        let skipped = from.checked_sub(self.start)?;
        let source = match self.source {
            MapSource::Reserved => MapSource::Reserved,
            MapSource::Backing { backing, offset } => {
                MapSource::Backing { backing, offset: BackingOffset::new(offset.get().checked_add(skipped)?) }
            }
        };
        (from <= self.last).then_some(Mapping { start: from, last: self.last, prot: self.prot, source })
    }

    /// `next` がすぐ後ろに並び、同じ権限で、同じ実体の続きを見せているか (一つの項目で表せるか)。
    fn continues_into(self, next: Mapping) -> bool {
        let adjacent = self.last.checked_add(1) == Some(next.start);
        let same_source = match (self.source, next.source) {
            (MapSource::Backing { backing, offset }, MapSource::Backing { backing: other, offset: follows }) => {
                let skipped = next.start.checked_sub(self.start);
                backing == other && skipped.and_then(|skipped| offset.get().checked_add(skipped)) == Some(follows.get())
            }
            (MapSource::Reserved, MapSource::Reserved | MapSource::Backing { .. })
            | (MapSource::Backing { .. }, MapSource::Reserved) => false,
        };
        adjacent && self.prot == next.prot && same_source
    }

    /// `range` の中に入る部分に、権限 `prot` を付けたもの。
    fn piece_within(self, start: u64, last: u64, prot: Prot) -> Option<Mapping> {
        let mut piece = self.tail_from(self.start.max(start))?;
        piece.last = piece.last.min(last);
        piece.prot = prot;
        Some(piece)
    }

    const fn backing(self) -> Option<BackingId> {
        match self.source {
            MapSource::Reserved => None,
            MapSource::Backing { backing, .. } => Some(backing),
        }
    }
}

/// 一つのマッピングの内容。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappingInfo {
    range: GuestRange,
    prot: Prot,
    source: MapSource,
}

impl MappingInfo {
    /// マッピングの範囲。
    #[must_use]
    pub const fn range(&self) -> GuestRange {
        self.range
    }

    /// 権限。予約だけのマッピングでは [`Prot::NONE`]。
    #[must_use]
    pub const fn prot(&self) -> Prot {
        self.prot
    }

    /// 中身。
    #[must_use]
    pub const fn source(&self) -> MapSource {
        self.source
    }
}

/// アクセスできる、実体の中の連続した区間。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    backing: BackingId,
    offset: BackingOffset,
    len: u64,
}

impl Span {
    /// 内容を持つ実体。
    #[must_use]
    pub const fn backing(&self) -> BackingId {
        self.backing
    }

    /// 要求した範囲の先頭に対応する、実体の中の位置。
    #[must_use]
    pub const fn offset(&self) -> BackingOffset {
        self.offset
    }

    /// 連続してアクセスできるバイト数 (1 以上)。要求より短ければ、続きを引き直す。
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.len
    }
}

/// ゲストのメモリアクセスの失敗。ゲストへ例外として渡すかどうかは Personality が決める。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemFault {
    addr: GuestAddr,
    kind: MemFaultKind,
}

impl MemFault {
    /// 実行器が自分で見つけた失敗を表す ([`crate::Stop::Fault`])。
    #[must_use]
    pub const fn new(addr: GuestAddr, kind: MemFaultKind) -> MemFault {
        MemFault { addr, kind }
    }

    /// 失敗した最初のアドレス。
    #[must_use]
    pub const fn addr(&self) -> GuestAddr {
        self.addr
    }

    /// 失敗の種類。
    #[must_use]
    pub const fn kind(&self) -> MemFaultKind {
        self.kind
    }
}

/// メモリアクセスの失敗の種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemFaultKind {
    /// マッピングが無い。
    Unmapped,
    /// 予約だけで実体が無い。
    Reserved,
    /// 権限が無い。
    Denied,
}

plain_try_clone!(Mapping, Backing);

impl TryClone for Space {
    fn try_clone(&self) -> Result<Space, Error> {
        let Space { width, page, maps, generation, limbo } = self;
        Ok(Space {
            width: *width,
            page: *page,
            maps: maps.try_clone()?,
            generation: *generation,
            limbo: limbo.try_clone()?,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Space {
    pub(crate) width: AddrWidth,
    pub(crate) page: PageSize,
    /// 先頭アドレスの昇順で、重ならない。
    pub(crate) maps: List<Mapping>,
    pub(crate) generation: u64,
    /// 外したが、古い世代の実行が残るので参照を持ち続けている実体。一つの実体は一度だけ入る。
    pub(crate) limbo: List<BackingId>,
}

impl Space {
    pub(crate) const fn new(width: AddrWidth, page: PageSize) -> Space {
        Space { width, page, maps: List::new(), generation: 0, limbo: List::new() }
    }

    pub(crate) const fn generation(&self) -> MapGen {
        MapGen(self.generation)
    }

    /// 実体つきで対応づけているバイト数。
    pub(crate) fn mapped_bytes(&self) -> u64 {
        let backed = self.maps.iter().filter(|mapping| matches!(mapping.source, MapSource::Backing { .. }));
        backed.fold(0u64, |total, mapping| {
            total.saturating_add(mapping.last.saturating_sub(mapping.start)).saturating_add(1)
        })
    }

    /// 範囲が粒度に揃い、幅に収まるか。
    fn check(&self, range: GuestRange) -> Result<(), Error> {
        if !self.page.divides(range.start().get()) || !self.page.divides(range.last().get().wrapping_add(1)) {
            return Err(Error::Refused(Refusal::Misaligned));
        }
        if range.last() > self.width.last() {
            return Err(Error::Refused(Refusal::OutOfSpace));
        }
        Ok(())
    }

    /// `addr` 以降に終わる最初のマッピングの位置。
    fn first_ending_at_or_after(&self, addr: u64) -> usize {
        self.maps.as_slice().partition_point(|mapping| mapping.last < addr)
    }

    fn at(&self, addr: u64) -> Option<&Mapping> {
        self.maps.get(self.first_ending_at_or_after(addr)).filter(|mapping| mapping.start <= addr)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Backing {
    pub(crate) len: u64,
    /// プロセスが持つ保持の合計。
    pub(crate) holds: u32,
    /// この実体を見せているマッピングの数。
    pub(crate) maps: u32,
    /// 保留しているアドレス空間の数 (一つの空間は一つの実体を一度だけ保留する)。
    pub(crate) limbo: u32,
    /// 通信路のメッセージが運んでいる保持の数。
    pub(crate) transit: u32,
}

/// `value` を `align` の倍数へ切り上げる。
fn align_up(value: u64, align: u64) -> Option<u64> {
    let mask = align.checked_sub(1)?;
    Some(value.checked_add(mask)? & !mask)
}

impl Kernel {
    /// 長さ `len` の実体を登録する。`process` が最初の保持者になる。
    ///
    /// 内容の確保はホスト実装の仕事である。確保できなければ保持を手放せばよい。
    pub fn create_backing(&mut self, process: ProcessId, len: u64) -> Result<BackingId, Error> {
        if len == 0 {
            return Err(Error::Refused(Refusal::EmptyBacking));
        }
        let total = self
            .backing_bytes
            .checked_add(len)
            .filter(|total| *total <= self.limits.backing_bytes())
            .ok_or(Error::Exhausted(Resource::BackingBytes))?;
        let limit = self.holdings_limit(process);
        let owner = self.processes.find_mut(process, &self.lineage)?;
        if owner.life != Life::Alive {
            return Err(Error::Refused(Refusal::ProcessExiting));
        }
        owner.backings.room_for_new(limit)?;
        self.backings.reserve()?;
        self.effects.promise(1)?;
        let backing = self.backings.insert(Backing { len, holds: 1, maps: 0, limbo: 0, transit: 0 })?;
        let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
        owner.backings.add(backing, limit)?;
        self.backing_bytes = total;
        Ok(backing)
    }

    /// `process` の保持を一つ増やす。
    pub fn retain_backing(&mut self, process: ProcessId, backing: BackingId) -> Result<(), Error> {
        let limit = self.holdings_limit(process);
        let holds = self.backings.find(backing, &self.lineage)?.holds;
        let holds = holds.checked_add(1).ok_or(Error::Refused(Refusal::HoldOverflow))?;
        let owner = self.processes.find_mut(process, &self.lineage)?;
        if owner.life != Life::Alive {
            return Err(Error::Refused(Refusal::ProcessExiting));
        }
        owner.backings.add(backing, limit)?;
        self.backings.get_mut(backing).ok_or(Error::Broken(Invariant::MemBackingAlive))?.holds = holds;
        Ok(())
    }

    /// `process` の保持を一つ減らす。マッピングや保留が残る間、実体は解放されない。
    pub fn release_backing(&mut self, process: ProcessId, backing: BackingId) -> Result<(), Error> {
        self.backings.find(backing, &self.lineage)?;
        let owner = self.processes.find_mut(process, &self.lineage)?;
        owner.backings.sub(backing)?;
        self.drop_backing_hold(backing)
    }

    /// `process` が実体つきで対応づけているバイト数 ([`crate::Quota::mapped_bytes`] が数える量)。
    pub fn mapped_bytes(&self, process: ProcessId) -> Result<u64, Error> {
        Ok(self.processes.find(process, &self.lineage)?.space.mapped_bytes())
    }

    /// 実体の長さ。
    pub fn backing_len(&self, backing: BackingId) -> Result<u64, Error> {
        Ok(self.backings.find(backing, &self.lineage)?.len)
    }

    /// 空いている範囲へマッピングを置く。既存のマッピングと重なれば拒否する。
    ///
    /// 予約だけのマッピングの権限は常に [`Prot::NONE`] になる。
    pub fn map(
        &mut self,
        process: ProcessId,
        range: GuestRange,
        source: MapSource,
        prot: Prot,
    ) -> Result<MapGen, Error> {
        let owner = self.processes.find(process, &self.lineage)?;
        if owner.life != Life::Alive {
            return Err(Error::Refused(Refusal::ProcessExiting));
        }
        let space = &owner.space;
        space.check(range)?;
        let (start, last) = (range.start().get(), range.last().get());
        let place = space.first_ending_at_or_after(start);
        if space.maps.get(place).is_some_and(|next| next.start <= last) {
            return Err(Error::Refused(Refusal::Overlap));
        }
        if u64::try_from(space.maps.len()).unwrap_or(u64::MAX) >= u64::from(self.mappings_limit(process)) {
            return Err(Error::Exhausted(Resource::Mappings));
        }
        let generation = space.generation.checked_add(1).ok_or(Error::Exhausted(Resource::Identifiers))?;
        let (prot, mapped) = match source {
            MapSource::Reserved => (Prot::NONE, None),
            MapSource::Backing { backing, offset } => {
                let entry = self.backings.find(backing, &self.lineage)?;
                if !space.page.divides(offset.get()) {
                    return Err(Error::Refused(Refusal::Misaligned));
                }
                let span = last.checked_sub(start).ok_or(Error::Broken(Invariant::MemLayout))?;
                if !offset.get().checked_add(span).is_some_and(|end| end < entry.len) {
                    return Err(Error::Refused(Refusal::BeyondBacking));
                }
                let maps = entry.maps.checked_add(1).ok_or(Error::Exhausted(Resource::Pins))?;
                let total = space.mapped_bytes().checked_add(span).and_then(|total| total.checked_add(1));
                if !total.is_some_and(|total| total <= owner.quota.mapped_bytes()) {
                    return Err(Error::Exhausted(Resource::MappedBytes));
                }
                (prot, Some((backing, maps)))
            }
        };
        let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
        owner.space.maps.insert(place, Mapping { start, last, prot, source }, Invariant::MemLayout)?;
        owner.space.generation = generation;
        if let Some((backing, maps)) = mapped {
            self.backings.get_mut(backing).ok_or(Error::Broken(Invariant::MemBackingAlive))?.maps = maps;
        }
        Ok(MapGen(generation))
    }

    /// 範囲の中のマッピングをすべて外す。範囲の端をまたぐマッピングは分割する。
    /// 外したバイト数を返す (`u64` に収まらなければ `u64::MAX`)。何も無ければ 0 で、
    /// 状態は変わらない。
    pub fn unmap(&mut self, process: ProcessId, range: GuestRange) -> Result<u64, Error> {
        let owner = self.processes.find(process, &self.lineage)?;
        if owner.life != Life::Alive {
            return Err(Error::Refused(Refusal::ProcessExiting));
        }
        let space = &owner.space;
        space.check(range)?;
        let (start, last) = (range.start().get(), range.last().get());
        let first = space.first_ending_at_or_after(start);
        // 計画: 何を外し、いくつ増え、いくつ保留が要るか。
        let mut bytes = 0u64;
        let mut cut_backed = 0u32;
        let mut split = false;
        for mapping in space.maps.iter().skip(first).take_while(|mapping| mapping.start <= last) {
            let from = mapping.start.max(start);
            let to = mapping.last.min(last);
            let piece = to.checked_sub(from).ok_or(Error::Broken(Invariant::MemLayout))?;
            bytes = bytes.saturating_add(piece).saturating_add(1);
            if mapping.start < start && mapping.last > last {
                split = true;
            }
            if let Some(backing) = mapping.backing() {
                cut_backed = cut_backed.checked_add(1).ok_or(Error::Exhausted(Resource::Pins))?;
                let entry = self.backings.get(backing).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
                // 分割で増えるマッピングと、保留の最大数を先に確かめる。
                if entry.maps == u32::MAX || entry.limbo.checked_add(cut_backed).is_none() {
                    return Err(Error::Exhausted(Resource::Pins));
                }
            }
        }
        if bytes == 0 {
            return Ok(0);
        }
        let generation = space.generation.checked_add(1).ok_or(Error::Exhausted(Resource::Identifiers))?;
        if split && u64::try_from(space.maps.len()).unwrap_or(u64::MAX) >= u64::from(self.mappings_limit(process)) {
            return Err(Error::Exhausted(Resource::Mappings));
        }
        let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
        owner.space.maps.reserve(usize::from(split))?;
        owner.space.limbo.reserve(usize::try_from(cut_backed).map_err(|_| Error::Exhausted(Resource::HostMemory))?)?;
        // 確定。権限が減るなら、先に古い世代の実行を数えて中断を要求する。
        let hold_old = cut_backed > 0 && self.reduce(process)?;
        let mut at = first;
        loop {
            let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
            let maps = &mut owner.space.maps;
            let Some(mapping) = maps.get(at).copied().filter(|mapping| mapping.start <= last) else { break };
            let keep_left = mapping.start < start;
            let right = last.checked_add(1).and_then(|from| mapping.tail_from(from));
            let mut more_maps = false;
            match (keep_left, right) {
                (false, None) => {
                    maps.remove(at).ok_or(Error::Broken(Invariant::MemLayout))?;
                }
                (true, None) => {
                    let entry = maps.get_mut(at).ok_or(Error::Broken(Invariant::MemLayout))?;
                    entry.last = start.checked_sub(1).ok_or(Error::Broken(Invariant::MemLayout))?;
                    at = at.saturating_add(1);
                }
                (false, Some(right)) => {
                    *maps.get_mut(at).ok_or(Error::Broken(Invariant::MemLayout))? = right;
                    at = at.saturating_add(1);
                }
                (true, Some(right)) => {
                    let entry = maps.get_mut(at).ok_or(Error::Broken(Invariant::MemLayout))?;
                    entry.last = start.checked_sub(1).ok_or(Error::Broken(Invariant::MemLayout))?;
                    maps.insert(at.saturating_add(1), right, Invariant::MemLayout)?;
                    at = at.saturating_add(2);
                    more_maps = true;
                }
            }
            let Some(backing) = mapping.backing() else { continue };
            // 保留は、一つの空間につき一つの実体を一度だけ持つ。古い世代の実行が残る間に同じ実体を
            // 何度外しても、記録は増えない。
            let hold = hold_old && !owner.space.limbo.iter().any(|held| *held == backing);
            if hold {
                owner.space.limbo.push_within(backing).map_err(|_| Error::Broken(Invariant::MemBackingAlive))?;
            }
            let entry = self.backings.get_mut(backing).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
            if hold {
                entry.limbo = entry.limbo.checked_add(1).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
            }
            if more_maps {
                entry.maps = entry.maps.checked_add(1).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
            } else if !keep_left && mapping.last <= last {
                entry.maps = entry.maps.checked_sub(1).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
            }
            self.reclaim_backing_if_unused(backing)?;
        }
        let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
        owner.space.generation = generation;
        Ok(bytes)
    }

    /// 範囲の権限を変える。範囲のすべてが実体のあるマッピングでなければ、何も変えずに
    /// 拒否する。範囲の端をまたぐマッピングは分割し、変更の後、隣り合う項目が同じ権限で
    /// 同じ実体の続きを見せていれば一つにまとめる。
    ///
    /// 範囲の全体が既に `prot` なら、何も変えずに今の世代を返す (分割も中断の要求もしない)。
    /// 権限が減る部分が無い変更 (増やすだけ) は、世代を進めるが、実行中のスレッドを止めない。
    /// 古い世代で検証したマッピングは、変更の前より狭い権限しか持たないからである。
    pub fn protect(&mut self, process: ProcessId, range: GuestRange, prot: Prot) -> Result<MapGen, Error> {
        let owner = self.processes.find(process, &self.lineage)?;
        if owner.life != Life::Alive {
            return Err(Error::Refused(Refusal::ProcessExiting));
        }
        let space = &owner.space;
        space.check(range)?;
        let (start, last) = (range.start().get(), range.last().get());
        let first = space.first_ending_at_or_after(start);
        let mut end = first;
        let mut expected = start;
        let mut changes = false;
        let mut reduces = false;
        loop {
            let mapping = space.maps.get(end).ok_or(Error::Refused(Refusal::NotMapped))?;
            if mapping.start > expected {
                return Err(Error::Refused(Refusal::NotMapped));
            }
            if mapping.backing().is_none() {
                return Err(Error::Refused(Refusal::NotBacked));
            }
            changes = changes || mapping.prot != prot;
            reduces = reduces || !prot.covers(mapping.prot);
            if mapping.last >= last {
                break;
            }
            expected = mapping.last.checked_add(1).ok_or(Error::Broken(Invariant::MemLayout))?;
            end = end.saturating_add(1);
        }
        if !changes {
            return Ok(space.generation());
        }
        let head = *space.maps.get(first).ok_or(Error::Broken(Invariant::MemLayout))?;
        let tail = *space.maps.get(end).ok_or(Error::Broken(Invariant::MemLayout))?;
        let middle = if head.start < start { head.tail_from(start) } else { None };
        let right = last.checked_add(1).and_then(|from| tail.tail_from(from));
        let extra = usize::from(middle.is_some()).saturating_add(usize::from(right.is_some()));
        // 分割で増えるマッピングの数を、実体ごとに先に確かめる。
        let grown = [middle.and_then(Mapping::backing), right.and_then(Mapping::backing)];
        for (at, backing) in grown.iter().enumerate() {
            let Some(backing) = backing else { continue };
            let earlier = u32::from(grown.iter().take(at).any(|other| other == &Some(*backing)));
            let entry = self.backings.get(*backing).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
            if entry.maps.checked_add(earlier).and_then(|maps| maps.checked_add(1)).is_none() {
                return Err(Error::Exhausted(Resource::Pins));
            }
        }
        // 変更の後にまとまる項目の数。範囲の中の部分と、その両隣 (分割で残る部分、無ければ隣の項目) を
        // 順に並べ、続いているものを数える。
        let before = match start.checked_sub(1) {
            Some(kept) if head.start < start => Some(Mapping { last: kept, ..head }),
            Some(_) | None => first.checked_sub(1).and_then(|at| space.maps.get(at).copied()),
        };
        let after = right.or_else(|| space.maps.get(end.saturating_add(1)).copied());
        let covered = end.saturating_sub(first).saturating_add(1);
        let mut merges = 0usize;
        let mut previous = before;
        for mapping in space.maps.iter().skip(first).take(covered) {
            let piece = mapping.piece_within(start, last, prot).ok_or(Error::Broken(Invariant::MemLayout))?;
            merges = merges.saturating_add(usize::from(previous.is_some_and(|earlier| earlier.continues_into(piece))));
            previous = Some(piece);
        }
        if let (Some(piece), Some(after)) = (previous, after) {
            merges = merges.saturating_add(usize::from(piece.continues_into(after)));
        }
        let count = space.maps.len().saturating_add(extra).saturating_sub(merges);
        if u64::try_from(count).unwrap_or(u64::MAX) > u64::from(self.mappings_limit(process)) {
            return Err(Error::Exhausted(Resource::Mappings));
        }
        let generation = space.generation.checked_add(1).ok_or(Error::Exhausted(Resource::Identifiers))?;
        let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
        let maps = &mut owner.space.maps;
        maps.reserve(extra)?;
        // 確定。後ろの分割を先に行い、前の位置がずれないようにする。
        if let Some(right) = right {
            maps.get_mut(end).ok_or(Error::Broken(Invariant::MemLayout))?.last = last;
            maps.insert(end.saturating_add(1), right, Invariant::MemLayout)?;
        }
        let mut from = first;
        if let Some(mut middle) = middle {
            // 範囲が一つのマッピングの内側なら、後ろの分割で末尾が縮んでいる。
            middle.last = middle.last.min(last);
            maps.get_mut(first).ok_or(Error::Broken(Invariant::MemLayout))?.last =
                start.checked_sub(1).ok_or(Error::Broken(Invariant::MemLayout))?;
            from = first.saturating_add(1);
            maps.insert(from, middle, Invariant::MemLayout)?;
        }
        for mapping in maps.iter_mut().skip(from).take_while(|mapping| mapping.start <= last) {
            mapping.prot = prot;
        }
        owner.space.generation = generation;
        for backing in grown.into_iter().flatten() {
            let entry = self.backings.get_mut(backing).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
            entry.maps = entry.maps.checked_add(1).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
        }
        // 続いている項目をまとめる。範囲の一つ手前から始め、範囲を過ぎたら終える。
        let mut at = from.saturating_sub(1);
        loop {
            let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
            let maps = &mut owner.space.maps;
            let following = at.saturating_add(1);
            let (Some(current), Some(next)) = (maps.get(at).copied(), maps.get(following).copied()) else { break };
            if current.start > last {
                break;
            }
            if !current.continues_into(next) {
                at = following;
                continue;
            }
            maps.get_mut(at).ok_or(Error::Broken(Invariant::MemLayout))?.last = next.last;
            maps.remove(following).ok_or(Error::Broken(Invariant::MemLayout))?;
            let backing = next.backing().ok_or(Error::Broken(Invariant::MemLayout))?;
            let entry = self.backings.get_mut(backing).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
            entry.maps = entry.maps.checked_sub(1).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
        }
        if reduces {
            self.reduce(process)?;
        }
        Ok(MapGen(generation))
    }

    /// 範囲の先頭から連続してアクセスできる実体の区間を引く。基準の経路。
    ///
    /// 外側の失敗は kome の呼び出しの失敗、内側の失敗はゲストのアクセスの失敗である。
    /// 返す区間が範囲より短いときは、マッピングの境界をまたいでいる。続きを引き直す。
    pub fn translate(
        &self,
        process: ProcessId,
        range: GuestRange,
        access: Access,
    ) -> Result<Result<Span, MemFault>, Error> {
        let space = &self.processes.find(process, &self.lineage)?.space;
        let addr = range.start();
        let fault = |kind| Ok(Err(MemFault { addr, kind }));
        let Some(mapping) = space.at(addr.get()) else { return fault(MemFaultKind::Unmapped) };
        let MapSource::Backing { backing, offset } = mapping.source else { return fault(MemFaultKind::Reserved) };
        if !mapping.prot.allows(access) {
            return fault(MemFaultKind::Denied);
        }
        let broken = Error::Broken(Invariant::MemLayout);
        let skipped = addr.get().checked_sub(mapping.start).ok_or(broken)?;
        let offset = BackingOffset::new(offset.get().checked_add(skipped).ok_or(broken)?);
        let until = mapping.last.min(range.last().get());
        let len = until.checked_sub(addr.get()).and_then(|span| span.checked_add(1)).ok_or(broken)?;
        Ok(Ok(Span { backing, offset, len }))
    }

    /// `addr` を含むマッピング。
    pub fn mapping_at(&self, process: ProcessId, addr: GuestAddr) -> Result<Option<MappingInfo>, Error> {
        Ok(self.next_mapping(process, addr)?.filter(|mapping| mapping.range.contains(addr)))
    }

    /// `from` 以降で最初のマッピング (`from` を含むものを含む)。アドレス空間を順に列挙するのに使う。
    pub fn next_mapping(&self, process: ProcessId, from: GuestAddr) -> Result<Option<MappingInfo>, Error> {
        let space = &self.processes.find(process, &self.lineage)?.space;
        let Some(mapping) = space.maps.get(space.first_ending_at_or_after(from.get())) else { return Ok(None) };
        let range = GuestRange::spanning(GuestAddr::new(mapping.start), GuestAddr::new(mapping.last))
            .ok_or(Error::Broken(Invariant::MemLayout))?;
        Ok(Some(MappingInfo { range, prot: mapping.prot, source: mapping.source }))
    }

    /// `within` の中で、`len` バイトが入る最も低い空き範囲の先頭。`align` と空間の粒度の
    /// 大きい方に揃える。置く方針 (下から、上から、決まった基底) は Personality が
    /// `within` で表す。
    pub fn find_free(
        &self,
        process: ProcessId,
        len: u64,
        align: PageSize,
        within: GuestRange,
    ) -> Result<Option<GuestAddr>, Error> {
        let space = &self.processes.find(process, &self.lineage)?.space;
        let Some(tail) = len.checked_sub(1) else { return Ok(None) };
        let align = align.bytes().max(space.page.bytes());
        let ceiling = within.last().get().min(space.width.last().get());
        let mut candidate = within.start().get();
        let mut at = space.first_ending_at_or_after(candidate);
        loop {
            let Some(start) = align_up(candidate, align) else { return Ok(None) };
            let Some(last) = start.checked_add(tail).filter(|last| *last <= ceiling) else { return Ok(None) };
            match space.maps.get(at) {
                Some(next) if next.start <= last => {
                    let Some(after) = next.last.checked_add(1) else { return Ok(None) };
                    candidate = after.max(candidate);
                    at = at.saturating_add(1);
                }
                Some(_) | None => return Ok(Some(GuestAddr::new(start))),
            }
        }
    }

    /// アドレス空間の世代。
    pub fn map_generation(&self, process: ProcessId) -> Result<MapGen, Error> {
        Ok(self.processes.find(process, &self.lineage)?.space.generation())
    }

    /// マッピングの数。
    pub fn mapping_count(&self, process: ProcessId) -> Result<usize, Error> {
        Ok(self.processes.find(process, &self.lineage)?.space.maps.len())
    }


    /// 権限を減らす変更に伴い、いま実行中のこのプロセスのスレッドを「古い世代」と数え、
    /// 中断を要求する。古い世代の実行が残っているかを返す。確保しない。
    fn reduce(&mut self, process: ProcessId) -> Result<bool, Error> {
        let mut newly = 0u32;
        let mut at = 0usize;
        while let Some(lent) = self.running.get(at).copied() {
            at = at.saturating_add(1);
            let thread = self.threads.get_mut(lent.thread).ok_or(Error::Broken(Invariant::RunOnePermit))?;
            if thread.process != process {
                continue;
            }
            match &mut thread.run {
                Run::Running { stale, .. } => {
                    if !*stale {
                        *stale = true;
                        newly = newly.saturating_add(1);
                    }
                }
                Run::Ready | Run::Servicing | Run::Blocked(_) => return Err(Error::Broken(Invariant::RunOnePermit)),
            }
            self.interrupt(lent.thread)?;
        }
        let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
        owner.stale_runners = owner.stale_runners.checked_add(newly).ok_or(Error::Broken(Invariant::MemBarrier))?;
        Ok(owner.stale_runners > 0)
    }

    /// 古い世代の実行が無くなったので、保留していた参照を手放す。
    pub(crate) fn flush_limbo(&mut self, process: ProcessId) -> Result<(), Error> {
        loop {
            let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
            let Some(backing) = owner.space.limbo.pop() else { return Ok(()) };
            let entry = self.backings.get_mut(backing).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
            entry.limbo = entry.limbo.checked_sub(1).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
            self.reclaim_backing_if_unused(backing)?;
        }
    }

    /// 終了するプロセスのマッピングをすべて外す。スレッドはもう無いので保留は要らない。
    pub(crate) fn clear_space(&mut self, process: ProcessId) -> Result<(), Error> {
        loop {
            let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
            if !owner.space.limbo.is_empty() {
                return Err(Error::Broken(Invariant::MemBarrier));
            }
            let Some(mapping) = owner.space.maps.pop() else { return Ok(()) };
            let Some(backing) = mapping.backing() else { continue };
            let entry = self.backings.get_mut(backing).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
            entry.maps = entry.maps.checked_sub(1).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
            self.reclaim_backing_if_unused(backing)?;
        }
    }

    /// 保持を一つ外す。プロセス側の記録は呼び出し側が既に減らしている。
    pub(crate) fn drop_backing_hold(&mut self, backing: BackingId) -> Result<(), Error> {
        let entry = self.backings.get_mut(backing).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
        entry.holds = entry.holds.checked_sub(1).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
        self.reclaim_backing_if_unused(backing)
    }

    pub(crate) fn reclaim_backing_if_unused(&mut self, backing: BackingId) -> Result<(), Error> {
        let entry = self.backings.get(backing).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
        if entry.holds != 0 || entry.maps != 0 || entry.limbo != 0 || entry.transit != 0 {
            return Ok(());
        }
        let len = entry.len;
        self.backings.remove(backing).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
        self.backing_bytes = self.backing_bytes.checked_sub(len).ok_or(Error::Broken(Invariant::MemBackingAlive))?;
        self.effects.emit(Effect::BackingReclaimed { backing })
    }
}
