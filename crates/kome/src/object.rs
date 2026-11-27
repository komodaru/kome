//! 待機可能な資源。
//!
//! kome が持つのは「数」と、待機が成立したときにその数を消費するかどうかだけである。
//! 手動・自動リセットのイベント、セマフォ、mutex の所有、放棄といった個々の OS の意味は
//! Personality がこの上に定義する。
//!
//! 参照は二種類ある。**保持** (`handles`) はプロセスが持つ参照で、ゲストがハンドルを
//! 閉じれば減る。**固定** (`pins`) は kome 自身の使用中の印で、待機・タイマ・未完了 I/O・
//! 生きているスレッドやプロセスの終了通知が持つ。両方が 0 になったとき初めて破棄する。

use crate::effect::Effect;
use crate::error::{Error, Invariant, Refusal, Resource};
use crate::id::{ObjectId, ProcessId, ThreadId};
use crate::kernel::Kernel;
use crate::process::Life;
use crate::store::{List, TryClone};

/// 待機が成立したときの扱い。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalMode {
    /// 数を変えない。数が 1 以上の間、すべての待機が成立する。
    Keep,
    /// 成立した待機一つにつき数を 1 減らす。
    Consume,
}

/// 待機可能な資源の定義。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignalSpec {
    mode: SignalMode,
    initial: u64,
    max: u64,
}

impl SignalSpec {
    /// `initial` から始まり `max` を超えない数を持つ資源。
    #[must_use]
    pub const fn new(mode: SignalMode, initial: u64, max: u64) -> SignalSpec {
        SignalSpec { mode, initial, max }
    }
}

/// 数の変更。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalOp {
    /// 数を増やす。上限を超えるなら何も変えずに拒否する。
    Add(u64),
    /// 数を置き換える。上限を超える値は拒否する。
    Set(u64),
}

impl TryClone for Object {
    fn try_clone(&self) -> Result<Object, Error> {
        let Object { mode, count, max, handles, pins, driven, waiters } = self;
        Ok(Object {
            mode: *mode,
            count: *count,
            max: *max,
            handles: *handles,
            pins: *pins,
            driven: *driven,
            waiters: waiters.try_clone()?,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Object {
    pub(crate) mode: SignalMode,
    pub(crate) count: u64,
    pub(crate) max: u64,
    pub(crate) handles: u32,
    pub(crate) pins: u32,
    /// 数を kome が決める資源 (通信路の通知)。外からの [`Kernel::signal`] を受け付けない。
    pub(crate) driven: bool,
    /// この資源を対象に持つ待機中のスレッド。登録順。
    pub(crate) waiters: List<ThreadId>,
}

impl Object {
    pub(crate) const fn new(spec: SignalSpec, handles: u32, pins: u32) -> Object {
        Object { mode: spec.mode, count: spec.initial, max: spec.max, handles, pins, driven: false, waiters: List::new() }
    }

    /// 通信路の通知。数は 0 か 1 で、待機が成立しても変わらない。通信路が一つ固定する。
    pub(crate) const fn driven(on: bool) -> Object {
        Object { mode: SignalMode::Keep, count: if on { 1 } else { 0 }, max: 1, handles: 0, pins: 1, driven: true, waiters: List::new() }
    }
}

impl Kernel {
    /// 待機可能な資源を作る。`process` が最初の保持者になる。
    pub fn create_object(&mut self, process: ProcessId, spec: SignalSpec) -> Result<ObjectId, Error> {
        if spec.max == 0 || spec.initial > spec.max {
            return Err(Error::Refused(Refusal::BadSignalSpec));
        }
        let limit = self.limits.holdings();
        let owner = self.processes.find_mut(process, &self.lineage)?;
        if owner.life != Life::Alive {
            return Err(Error::Refused(Refusal::ProcessExiting));
        }
        owner.objects.room_for_new(limit)?;
        self.objects.reserve()?;
        self.effects.promise(1)?;
        let object = self.objects.insert(Object::new(spec, 1, 0))?;
        let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
        owner.objects.add(object, limit)?;
        Ok(object)
    }

    /// `process` の保持を一つ増やす。
    pub fn retain_object(&mut self, process: ProcessId, object: ObjectId) -> Result<(), Error> {
        let limit = self.limits.holdings();
        let handles = self.objects.find(object, &self.lineage)?.handles;
        let handles = handles.checked_add(1).ok_or(Error::Refused(Refusal::HoldOverflow))?;
        let owner = self.processes.find_mut(process, &self.lineage)?;
        if owner.life != Life::Alive {
            return Err(Error::Refused(Refusal::ProcessExiting));
        }
        owner.objects.add(object, limit)?;
        self.objects.get_mut(object).ok_or(Error::Broken(Invariant::ObjectRefs))?.handles = handles;
        Ok(())
    }

    /// `process` の保持を一つ減らす。保持も固定も無くなれば破棄する。
    pub fn release_object(&mut self, process: ProcessId, object: ObjectId) -> Result<(), Error> {
        self.objects.find(object, &self.lineage)?;
        let owner = self.processes.find_mut(process, &self.lineage)?;
        owner.objects.sub(object)?;
        self.drop_handle(object)
    }

    /// 数を変更し、成立した待機を登録順に確定する。変更前の数を返す。
    pub fn signal(&mut self, object: ObjectId, op: SignalOp) -> Result<u64, Error> {
        let entry = self.objects.find_mut(object, &self.lineage)?;
        if entry.driven {
            return Err(Error::Refused(Refusal::DrivenObject));
        }
        let previous = entry.count;
        let next = match op {
            SignalOp::Add(amount) => previous.checked_add(amount).filter(|next| *next <= entry.max),
            SignalOp::Set(value) => Some(value).filter(|next| *next <= entry.max),
        };
        entry.count = next.ok_or(Error::Refused(Refusal::SignalOverflow))?;
        self.settle(object)?;
        Ok(previous)
    }

    /// 現在の数。
    pub fn signal_count(&self, object: ObjectId) -> Result<u64, Error> {
        Ok(self.objects.find(object, &self.lineage)?.count)
    }

    /// 数を 1 増やす (上限で止まる)。kome 自身の通知 (タイマ、I/O、終了) に使う。
    pub(crate) fn raise(&mut self, object: ObjectId) -> Result<(), Error> {
        let entry = self.objects.get_mut(object).ok_or(Error::Broken(Invariant::ObjectRefs))?;
        entry.count = entry.count.saturating_add(1).min(entry.max);
        self.settle(object)
    }

    /// `extra` 個の固定を追加できるか。受理前の検査。
    pub(crate) fn can_pin(&self, object: ObjectId, extra: u32) -> Result<(), Error> {
        let entry = self.objects.find(object, &self.lineage)?;
        entry.pins.checked_add(extra).map(|_| ()).ok_or(Error::Exhausted(Resource::Pins))
    }

    /// kome が数を増やす通知先 (タイマ、I/O) として、`extra` 個の固定を追加できるか。
    /// 数を kome が中身から決める資源 (通信路の通知) は、通知先にできない。
    pub(crate) fn can_notify(&self, object: ObjectId, extra: u32) -> Result<(), Error> {
        if self.objects.find(object, &self.lineage)?.driven {
            return Err(Error::Refused(Refusal::DrivenObject));
        }
        self.can_pin(object, extra)
    }

    /// 固定を一つ追加する。[`Kernel::can_pin`] で確かめた後に呼ぶ。
    pub(crate) fn pin(&mut self, object: ObjectId) -> Result<(), Error> {
        let entry = self.objects.get_mut(object).ok_or(Error::Broken(Invariant::ObjectRefs))?;
        entry.pins = entry.pins.checked_add(1).ok_or(Error::Broken(Invariant::ObjectRefs))?;
        Ok(())
    }

    /// 固定を一つ外す。保持も固定も無くなれば破棄する。
    pub(crate) fn unpin(&mut self, object: ObjectId) -> Result<(), Error> {
        let entry = self.objects.get_mut(object).ok_or(Error::Broken(Invariant::ObjectRefs))?;
        entry.pins = entry.pins.checked_sub(1).ok_or(Error::Broken(Invariant::ObjectRefs))?;
        self.reclaim_object_if_unused(object)
    }

    /// 保持を一つ外す。プロセス側の記録は呼び出し側が既に減らしている。
    pub(crate) fn drop_handle(&mut self, object: ObjectId) -> Result<(), Error> {
        let entry = self.objects.get_mut(object).ok_or(Error::Broken(Invariant::ObjectRefs))?;
        entry.handles = entry.handles.checked_sub(1).ok_or(Error::Broken(Invariant::ObjectRefs))?;
        self.reclaim_object_if_unused(object)
    }

    fn reclaim_object_if_unused(&mut self, object: ObjectId) -> Result<(), Error> {
        let entry = self.objects.get(object).ok_or(Error::Broken(Invariant::ObjectRefs))?;
        if entry.handles != 0 || entry.pins != 0 {
            return Ok(());
        }
        if !entry.waiters.is_empty() {
            return Err(Error::Broken(Invariant::WaitOnce));
        }
        self.objects.remove(object).ok_or(Error::Broken(Invariant::ObjectRefs))?;
        self.effects.emit(Effect::ObjectReclaimed { object })
    }
}
