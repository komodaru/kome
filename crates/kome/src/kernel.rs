//! [`Kernel`]: 一つの実行インスタンスの制御状態。

use crate::effect::{Effect, Effects};
use crate::error::{Error, Refusal};
use crate::id::{Arena, ThreadId, TimerId, kind};
use crate::io::Io;
use crate::limits::Limits;
use crate::mem::Backing;
use crate::object::Object;
use crate::process::Process;
use crate::sched::WorkerId;
use crate::store::{List, Queue};
use crate::thread::Thread;
use crate::time::Instant;
use crate::timer::Timer;

/// 実行インスタンスの識別。復元した状態は、復元前とは別の値を持たなければならない。
///
/// 実行権と完了通知はこの値を持ち、別のインスタンスへ渡すと [`Error::Foreign`] になる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstanceId(u64);

impl InstanceId {
    /// 数値から作る。一意に選ぶのは呼び出し側の責任である。
    #[must_use]
    pub const fn new(value: u64) -> InstanceId {
        InstanceId(value)
    }

    /// 識別の数値。
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// 貸し出し中の実行権の記録。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Lent {
    pub(crate) worker: WorkerId,
    pub(crate) thread: ThreadId,
}

/// 期限が来たときに起こすもの。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Due {
    Wait(ThreadId),
    Timer(TimerId),
}

/// 期限の列の一項目。列は (時刻, 登録順) の昇順。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DueEntry {
    pub(crate) at: Instant,
    pub(crate) seq: u64,
    pub(crate) what: Due,
}

/// 共通カーネルの制御状態。
///
/// 一つの実行主体が `&mut` で更新する。同じ初期状態と同じ入力列から、同じ状態と
/// 同じ [`Effect`] 列を得る。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kernel {
    pub(crate) instance: InstanceId,
    pub(crate) limits: Limits,
    pub(crate) now: Instant,
    /// 登録順の番号。期限の列で同時刻の順序を決める。
    pub(crate) seq: u64,
    pub(crate) processes: Arena<kind::Process, Process>,
    pub(crate) threads: Arena<kind::Thread, Thread>,
    pub(crate) objects: Arena<kind::Object, Object>,
    pub(crate) backings: Arena<kind::Backing, Backing>,
    pub(crate) timers: Arena<kind::Timer, Timer>,
    pub(crate) ios: Arena<kind::Io, Io>,
    /// 実行可能なスレッド。先入れ先出し。
    pub(crate) ready: Queue<ThreadId>,
    pub(crate) running: List<Lent>,
    pub(crate) due: List<DueEntry>,
    pub(crate) effects: Effects,
    pub(crate) backing_bytes: u64,
}

impl Kernel {
    /// 空の状態を作る。`Effect` のキューと実行権の表は、ここで上限ぶんを確保する。
    pub fn new(instance: InstanceId, limits: Limits) -> Result<Kernel, Error> {
        if limits.quantum().get() == 0 {
            return Err(Error::Refused(Refusal::BadLimits));
        }
        let mut running = List::new();
        running.reserve_total(usize::try_from(limits.workers()).unwrap_or(usize::MAX))?;
        Ok(Kernel {
            instance,
            limits,
            now: Instant::ZERO,
            seq: 0,
            processes: Arena::new(limits.processes()),
            threads: Arena::new(limits.threads()),
            objects: Arena::new(limits.objects()),
            backings: Arena::new(limits.backings()),
            timers: Arena::new(limits.timers()),
            ios: Arena::new(limits.ios()),
            ready: Queue::new(),
            running,
            due: List::new(),
            effects: Effects::new(limits.effects())?,
            backing_bytes: 0,
        })
    }

    /// この状態の実行インスタンス。
    #[must_use]
    pub const fn instance(&self) -> InstanceId {
        self.instance
    }

    /// 上限。
    #[must_use]
    pub const fn limits(&self) -> &Limits {
        &self.limits
    }

    /// 現在の仮想時刻。
    #[must_use]
    pub const fn now(&self) -> Instant {
        self.now
    }

    /// 外へ求める処理を、出した順に一つ取り出す。
    pub fn poll_effect(&mut self) -> Option<Effect> {
        self.effects.poll()
    }

    /// 取り出されていない `Effect` の数。
    #[must_use]
    pub fn pending_effects(&self) -> usize {
        self.effects.queued()
    }

    /// 次の登録順の番号。
    pub(crate) fn next_seq(&mut self) -> u64 {
        let seq = self.seq;
        self.seq = self.seq.saturating_add(1);
        seq
    }
}
