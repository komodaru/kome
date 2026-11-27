//! スレッド: 実行資格、待機、停止要求、終了。
//!
//! 状態は一つの排他的な列挙ではなく、独立した三つの部分で表す。
//!
//! - **実行**: 実行待ち / 実行中 (実行権を貸している) / サービス中 (Personality が
//!   API 呼び出しや例外を処理している) / 待機中。
//! - **停止要求**: 停止要求の数。待機中のスレッドにも停止を要求できる。
//! - **終了要求**: 実行中のスレッドへの終了要求は、実行権が返った時点で効く。
//!
//! TEB・TLS の配置、APC、signal、例外の規則は Personality が持つ。

use crate::effect::Effect;
use crate::error::{Error, Invariant, Refusal, Resource};
use crate::id::{ObjectId, ProcessId, ThreadId};
use crate::kernel::Kernel;
use crate::object::{Object, SignalMode, SignalSpec};
use crate::process::{ExitCode, Life};
use crate::sched::{Priority, WorkerId};
use crate::store::TryClone;
use crate::wait::Wait;

/// 作った直後のスレッドの状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    /// すぐに実行待ちへ入る。
    Ready,
    /// 停止要求を一つ持った状態で始まる。
    Suspended,
}

impl TryClone for Run {
    fn try_clone(&self) -> Result<Run, Error> {
        Ok(match self {
            Run::Ready => Run::Ready,
            Run::Running { worker, stale, interrupted } => {
                Run::Running { worker: *worker, stale: *stale, interrupted: *interrupted }
            }
            Run::Servicing => Run::Servicing,
            Run::Blocked(wait) => Run::Blocked(wait.try_clone()?),
        })
    }
}

impl TryClone for Thread {
    fn try_clone(&self) -> Result<Thread, Error> {
        let Thread { process, exit_signal, run, suspend, kill, run_gen, executed, priority } = self;
        Ok(Thread {
            process: *process,
            exit_signal: *exit_signal,
            run: run.try_clone()?,
            suspend: *suspend,
            kill: *kill,
            run_gen: *run_gen,
            executed: *executed,
            priority: *priority,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Run {
    /// 実行権を待っている。
    Ready,
    /// 実行権を貸している。
    Running {
        worker: WorkerId,
        /// 権限を減らす変更より前の世代で実行しているか。
        stale: bool,
        /// この実行に対して中断を要求済みか。
        interrupted: bool,
    },
    /// Personality が処理している。実行権は貸さない。
    Servicing,
    /// 待機を登録している。
    Blocked(Wait),
}

/// スレッドの実行の状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadPhase {
    /// 実行権を待っている。
    Ready,
    /// 実行権を貸している。
    Running,
    /// Personality が処理している。
    Servicing,
    /// 待機している。
    Blocked,
}

/// スレッドが作られたときに返すもの: スレッドと、終了時に数が 1 になる資源。
pub type ThreadCreated = (ThreadId, ObjectId);

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Thread {
    pub(crate) process: ProcessId,
    pub(crate) exit_signal: ObjectId,
    pub(crate) run: Run,
    pub(crate) suspend: u32,
    pub(crate) kill: Option<ExitCode>,
    /// 実行権を貸した回数。貸すたびに進む。
    pub(crate) run_gen: u64,
    /// 実行した量の累計。
    pub(crate) executed: u64,
    pub(crate) priority: Priority,
}

impl Kernel {
    /// スレッドと、その終了時に数が 1 になる資源を作る。
    pub fn create_thread(&mut self, process: ProcessId, start: Start) -> Result<ThreadCreated, Error> {
        let owner = self.processes.find_mut(process, &self.lineage)?;
        if owner.life != Life::Alive {
            return Err(Error::Refused(Refusal::ProcessExiting));
        }
        if u64::try_from(owner.threads.len()).unwrap_or(u64::MAX) >= u64::from(owner.quota.threads()) {
            return Err(Error::Exhausted(Resource::ProcessThreads));
        }
        owner.threads.reserve(1)?;
        self.threads.reserve()?;
        self.objects.reserve()?;
        let total = usize::try_from(self.threads.len()).unwrap_or(usize::MAX).saturating_add(1);
        self.ready.reserve_total(total)?;
        // 終了通知の資源の破棄と、スレッドの終了。
        self.effects.promise(2)?;
        let exit_signal = self.objects.insert(Object::new(SignalSpec::new(SignalMode::Keep, 0, 1), 0, 1))?;
        let suspend = match start {
            Start::Ready => 0,
            Start::Suspended => 1,
        };
        let thread = self.threads.insert(Thread {
            process,
            exit_signal,
            run: Run::Ready,
            suspend,
            kill: None,
            run_gen: 0,
            executed: 0,
            priority: Priority::DEFAULT,
        })?;
        let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
        owner.threads.push(thread)?;
        if suspend == 0 {
            self.enqueue(thread)?;
        }
        Ok((thread, exit_signal))
    }

    /// サービス中のスレッドを実行待ちへ戻す。
    pub fn continue_thread(&mut self, thread: ThreadId) -> Result<(), Error> {
        let entry = self.threads.find_mut(thread, &self.lineage)?;
        if entry.run != Run::Servicing {
            return Err(Error::Refused(Refusal::NotServicing));
        }
        entry.run = Run::Ready;
        if entry.suspend == 0 { self.enqueue(thread) } else { Ok(()) }
    }

    /// 停止要求を一つ増やす。増やす前の数を返す。
    ///
    /// 実行中のスレッドには中断を要求する。止まるのは実行権が返った時点である。
    pub fn suspend_thread(&mut self, thread: ThreadId) -> Result<u32, Error> {
        let entry = self.threads.find_mut(thread, &self.lineage)?;
        let previous = entry.suspend;
        entry.suspend = previous.checked_add(1).ok_or(Error::Refused(Refusal::SuspendOverflow))?;
        match entry.run {
            Run::Ready if previous == 0 => self.dequeue(thread),
            Run::Running { .. } => self.interrupt(thread),
            Run::Ready | Run::Servicing | Run::Blocked(_) => Ok(()),
        }?;
        Ok(previous)
    }

    /// 停止要求を一つ減らす。減らす前の数を返す。
    pub fn unsuspend_thread(&mut self, thread: ThreadId) -> Result<u32, Error> {
        let entry = self.threads.find_mut(thread, &self.lineage)?;
        let previous = entry.suspend;
        entry.suspend = previous.checked_sub(1).ok_or(Error::Refused(Refusal::NotSuspended))?;
        if entry.suspend == 0 && entry.run == Run::Ready {
            self.enqueue(thread)?;
        }
        Ok(previous)
    }

    /// スレッドを終了させる。実行中なら、実行権が返った時点で終了する。
    ///
    /// プロセスの最後のスレッドが終了すると、プロセスも同じ終了コードで終了する。
    pub fn terminate_thread(&mut self, thread: ThreadId, code: ExitCode) -> Result<(), Error> {
        let entry = self.threads.find_mut(thread, &self.lineage)?;
        match entry.run {
            Run::Running { .. } => {
                entry.kill = entry.kill.or(Some(code));
                self.interrupt(thread)
            }
            Run::Ready | Run::Servicing | Run::Blocked(_) => self.exit_thread_now(thread, code),
        }
    }

    /// スレッドの実行の状態。
    pub fn thread_phase(&self, thread: ThreadId) -> Result<ThreadPhase, Error> {
        let entry = self.threads.find(thread, &self.lineage)?;
        Ok(match entry.run {
            Run::Ready => ThreadPhase::Ready,
            Run::Running { .. } => ThreadPhase::Running,
            Run::Servicing => ThreadPhase::Servicing,
            Run::Blocked(_) => ThreadPhase::Blocked,
        })
    }

    /// 停止要求の数。
    pub fn suspend_count(&self, thread: ThreadId) -> Result<u32, Error> {
        Ok(self.threads.find(thread, &self.lineage)?.suspend)
    }

    /// スレッドが所属するプロセス。
    pub fn process_of(&self, thread: ThreadId) -> Result<ProcessId, Error> {
        Ok(self.threads.find(thread, &self.lineage)?.process)
    }

    /// スレッドが実行した量の累計。
    pub fn executed(&self, thread: ThreadId) -> Result<u64, Error> {
        Ok(self.threads.find(thread, &self.lineage)?.executed)
    }

    /// 実行待ちの列の末尾へ入れる。容量は [`Kernel::create_thread`] が確保してある。
    pub(crate) fn enqueue(&mut self, thread: ThreadId) -> Result<(), Error> {
        self.ready.push_back_within(thread).map_err(|_| Error::Broken(Invariant::RunQueue))
    }

    fn dequeue(&mut self, thread: ThreadId) -> Result<(), Error> {
        let at = self.ready.position(|queued| *queued == thread).ok_or(Error::Broken(Invariant::RunQueue))?;
        self.ready.remove(at).map(|_| ()).ok_or(Error::Broken(Invariant::RunQueue))
    }

    /// 実行中のスレッドの worker へ中断を要求する。一度の実行につき一度だけ出す。
    pub(crate) fn interrupt(&mut self, thread: ThreadId) -> Result<(), Error> {
        let entry = self.threads.get_mut(thread).ok_or(Error::Broken(Invariant::RunOnePermit))?;
        let run = crate::sched::RunGen::new(entry.run_gen);
        match &mut entry.run {
            Run::Running { worker, interrupted, .. } if !*interrupted => {
                *interrupted = true;
                let worker = *worker;
                // 席は実行権を貸すときに予約してある。
                self.effects.emit(Effect::Interrupt { worker, thread, run })
            }
            Run::Running { .. } | Run::Ready | Run::Servicing | Run::Blocked(_) => Ok(()),
        }
    }

    /// 実行中でないスレッドを終了させ、記録を回収する。
    pub(crate) fn exit_thread_now(&mut self, thread: ThreadId, code: ExitCode) -> Result<(), Error> {
        if matches!(self.threads.get(thread).map(|entry| &entry.run), Some(Run::Running { .. }) | None) {
            return Err(Error::Broken(Invariant::RunOnePermit));
        }
        let entry = self.threads.remove(thread).ok_or(Error::Broken(Invariant::ProcessTree))?;
        match entry.run {
            Run::Ready => {
                if entry.suspend == 0 {
                    self.dequeue(thread)?;
                }
            }
            Run::Blocked(wait) => {
                self.detach_wait(thread, &wait)?;
                self.effects.forgo(1)?;
            }
            Run::Servicing => {}
            Run::Running { .. } => return Err(Error::Broken(Invariant::RunOnePermit)),
        }
        let process = entry.process;
        let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
        owner.threads.remove_first(|member| *member == thread).ok_or(Error::Broken(Invariant::ProcessTree))?;
        let last = owner.threads.is_empty();
        if last && owner.life == Life::Alive {
            owner.life = Life::Exiting(code);
        }
        self.raise(entry.exit_signal)?;
        self.unpin(entry.exit_signal)?;
        self.effects.emit(Effect::ThreadExited { thread, process, code })?;
        if last { self.finish_process(process) } else { Ok(()) }
    }
}
