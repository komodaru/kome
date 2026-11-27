//! 通知だけを見ている backend の代わり: アドレス空間の写しと、実体の内容。
//!
//! [`Shadow`] は、kome の通知 ([`kome::SpaceObserver`]) だけからアドレス空間を組み立てる。決定的
//! シミュレーションは、これが独立したモデル ([`crate::pages`]) と同じになることを確かめる。
//! 同じなら、通知だけを見ている backend は kome と同じアドレス空間を保てる。
//!
//! [`Store`] は、書かれたバイトだけを持つ内容の置き場である ([`kome::Contents`])。

use std::collections::BTreeMap;

use kome::{Access, BackingId, BackingOffset, Contents, GuestRange, HostError, MapChange, MapGen, MapSource, ProcessId, Prot, SpaceObserver};

use crate::pages::{Pages, Perm};

/// 通知から組み立てたアドレス空間。
#[derive(Clone, Debug, Default)]
pub struct Shadow {
    page: u64,
    spaces: BTreeMap<ProcessId, Pages>,
    generations: BTreeMap<ProcessId, MapGen>,
    cleared: u64,
}

fn perm(prot: Prot) -> Perm {
    let bit = |access, value| if prot.allows(access) { value } else { 0 };
    Perm::from_bits(bit(Access::Read, 1) | bit(Access::Write, 2) | bit(Access::Execute, 4))
}

impl Shadow {
    /// ページの大きさ `page` で数える、空の写し。
    #[must_use]
    pub fn new(page: u64) -> Shadow {
        Shadow { page, ..Shadow::default() }
    }

    /// `process` のアドレス空間の写し。一度も通知を受けていなければ `None`。
    #[must_use]
    pub fn space(&self, process: ProcessId) -> Option<&Pages> {
        self.spaces.get(&process)
    }

    /// `process` について最後に知らされた世代。
    #[must_use]
    pub fn generation(&self, process: ProcessId) -> Option<MapGen> {
        self.generations.get(&process).copied()
    }

    /// アドレス空間が無くなったと知らされた回数。
    #[must_use]
    pub const fn cleared_count(&self) -> u64 {
        self.cleared
    }

    fn pages(&self, range: GuestRange) -> (u64, u64) {
        let first = range.start().get().checked_div(self.page).unwrap_or(0);
        let last = range.last().get().checked_div(self.page).unwrap_or(0);
        (first, last.saturating_sub(first).saturating_add(1))
    }
}

impl SpaceObserver for Shadow {
    fn changed(&mut self, process: ProcessId, generation: MapGen, change: MapChange) {
        let (first, count) = self.pages(change.range());
        let page = self.page;
        let space = self.spaces.entry(process).or_default();
        match change {
            MapChange::Mapped { source, prot, .. } => {
                let source = match source {
                    MapSource::Reserved => None,
                    MapSource::Backing { backing, offset } => Some((backing, offset.get())),
                };
                space.map(first, count, source, perm(prot), page);
            }
            MapChange::Unmapped { .. } => {
                space.unmap(first, count);
            }
            MapChange::Protected { prot, .. } => {
                space.protect(first, count, perm(prot));
            }
        }
        self.generations.insert(process, generation);
    }

    fn cleared(&mut self, process: ProcessId) {
        self.spaces.remove(&process);
        self.generations.remove(&process);
        self.cleared = self.cleared.saturating_add(1);
    }
}

/// 書かれたバイトだけを持つ、実体の内容の置き場。書かれていない位置は 0 と読める。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Store {
    bytes: BTreeMap<(BackingId, u64), u8>,
}

impl Store {
    /// 実体 `backing` の `offset` のバイト。
    #[must_use]
    pub fn byte(&self, backing: BackingId, offset: u64) -> u8 {
        self.bytes.get(&(backing, offset)).copied().unwrap_or(0)
    }

    /// 実体 `backing` の `offset` へ一バイト書く。
    pub fn set(&mut self, backing: BackingId, offset: u64, value: u8) {
        self.bytes.insert((backing, offset), value);
    }
}

impl Contents for Store {
    fn read(&self, backing: BackingId, offset: BackingOffset, out: &mut [u8]) -> Result<(), HostError> {
        for (at, slot) in (offset.get()..).zip(out.iter_mut()) {
            *slot = self.byte(backing, at);
        }
        Ok(())
    }

    fn write(&mut self, backing: BackingId, offset: BackingOffset, data: &[u8]) -> Result<(), HostError> {
        for (at, value) in (offset.get()..).zip(data) {
            self.set(backing, at, *value);
        }
        Ok(())
    }
}
