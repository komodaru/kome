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
use crate::store::{List, Queue, TryClone, plain_try_clone};
use crate::thread::Thread;
use crate::time::Instant;
use crate::timer::Timer;

/// 実行インスタンスの識別。復元した状態は、復元前とは別の値を持つ ([`Snapshot::restore`])。
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

/// 生きている資源の数。資源が回収されたことを外から確かめるのに使う。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Census {
    processes: u32,
    threads: u32,
    objects: u32,
    backings: u32,
    timers: u32,
    ios: u32,
}

impl Census {
    /// プロセスの数。
    #[must_use]
    pub const fn processes(&self) -> u32 {
        self.processes
    }

    /// スレッドの数。
    #[must_use]
    pub const fn threads(&self) -> u32 {
        self.threads
    }

    /// 待機可能な資源の数。
    #[must_use]
    pub const fn objects(&self) -> u32 {
        self.objects
    }

    /// メモリの実体の数。
    #[must_use]
    pub const fn backings(&self) -> u32 {
        self.backings
    }

    /// タイマの数。
    #[must_use]
    pub const fn timers(&self) -> u32 {
        self.timers
    }

    /// I/O 要求の数。
    #[must_use]
    pub const fn ios(&self) -> u32 {
        self.ios
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

plain_try_clone!(Lent, DueEntry);

/// 共通カーネルの制御状態。
///
/// 一つの実行主体が `&mut` で更新する。同じ初期状態と同じ入力列から、同じ状態と
/// 同じ [`Effect`] 列を得る。
///
/// 複製できない。同じ実行インスタンスの状態が二つあると、一つの実行権を両方が受け取れて
/// しまう。状態を残すには [`Kernel::snapshot`] で写しを取り、別の実行インスタンスとして
/// [`Snapshot::restore`] で復元する。
///
/// ```compile_fail
/// fn twice(kernel: &kome::Kernel) -> kome::Kernel { kernel.clone() }
/// ```
#[derive(Debug, PartialEq, Eq)]
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

    /// 生きている資源の数。
    #[must_use]
    pub fn census(&self) -> Census {
        Census {
            processes: self.processes.len(),
            threads: self.threads.len(),
            objects: self.objects.len(),
            backings: self.backings.len(),
            timers: self.timers.len(),
            ios: self.ios.len(),
        }
    }

    /// 状態の写しを取る。確保に失敗したら `Exhausted(HostMemory)` を返す。
    pub fn snapshot(&self) -> Result<Snapshot, Error> {
        Ok(Snapshot { state: self.duplicate(self.instance)? })
    }

    fn duplicate(&self, instance: InstanceId) -> Result<Kernel, Error> {
        let Kernel {
            instance: _,
            limits,
            now,
            seq,
            processes,
            threads,
            objects,
            backings,
            timers,
            ios,
            ready,
            running,
            due,
            effects,
            backing_bytes,
        } = self;
        Ok(Kernel {
            instance,
            limits: *limits,
            now: *now,
            seq: *seq,
            processes: processes.try_clone()?,
            threads: threads.try_clone()?,
            objects: objects.try_clone()?,
            backings: backings.try_clone()?,
            timers: timers.try_clone()?,
            ios: ios.try_clone()?,
            ready: ready.try_clone()?,
            running: running.try_clone()?,
            due: due.try_clone()?,
            effects: effects.try_clone()?,
            backing_bytes: *backing_bytes,
        })
    }

    /// 取り出されていない `Effect` の `at` 番目 (0 が次に取り出すもの) を、取り出さずに見る。
    #[must_use]
    pub fn peek_effect(&self, at: usize) -> Option<Effect> {
        self.effects.peek(at)
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

/// ある時点の [`Kernel`] の写し。比べることと、別の実行インスタンスとして復元することだけができる。
///
/// 写しそのものは操作を受け付けない。下の二つは `Kernel` か `Snapshot` かだけが違う。
///
/// ```
/// fn step(kernel: &mut kome::Kernel) -> Option<kome::Effect> { kernel.poll_effect() }
/// ```
///
/// ```compile_fail
/// fn step(kernel: &mut kome::Snapshot) -> Option<kome::Effect> { kernel.poll_effect() }
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot {
    state: Kernel,
}

impl Snapshot {
    /// 写しを取った実行インスタンス。
    #[must_use]
    pub const fn instance(&self) -> InstanceId {
        self.state.instance
    }

    /// 写しを、実行インスタンス `instance` の状態として復元する。
    ///
    /// 復元した状態は、写しを取った実行インスタンスの実行権と完了通知を
    /// [`Error::Foreign`] で拒否する。そのため、外に貸しや依頼が残っている写しは復元できない:
    ///
    /// - `instance` が写しと同じなら `Refused(SameInstance)`。
    /// - 実行権を貸したままなら `Refused(PermitsLent)`。
    /// - 発行済みでホストの応答を待つ I/O があれば `Refused(IoInFlight)`。
    ///
    /// 取り出されていない [`Effect`] は復元した状態に残り、復元先のホストが取り出す。
    pub fn restore(&self, instance: InstanceId) -> Result<Kernel, Error> {
        if instance == self.state.instance {
            return Err(Error::Refused(Refusal::SameInstance));
        }
        if !self.state.running.is_empty() {
            return Err(Error::Refused(Refusal::PermitsLent));
        }
        if self.state.ios.iter().any(|(_, io)| io.state.awaits_host()) {
            return Err(Error::Refused(Refusal::IoInFlight));
        }
        self.state.duplicate(instance)
    }
}

impl PartialEq<Snapshot> for Kernel {
    fn eq(&self, other: &Snapshot) -> bool {
        *self == other.state
    }
}
