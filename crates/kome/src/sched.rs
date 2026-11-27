//! 実行権: ゲスト命令を実行する worker へ貸す、複製できない資格。
//!
//! 制御状態を更新するのは単一 writer だが、ゲストの命令実行とメモリアクセスは worker が
//! 行う。kome は worker に [`RunPermit`] を貸し、貸し出しを自分でも記録する。worker は
//! 理由を付けて実行権を返す。kome は対象と世代を照合し、二重実行と古い実行結果の受理を
//! 防ぐ (台帳 RUN-1)。
//!
//! 実行権は複製できない。下の二つは `Budget` (複製できる) か `RunPermit` かだけが違う。
//!
//! ```
//! fn twice(budget: kome::Budget) -> (kome::Budget, kome::Budget) { (budget.clone(), budget) }
//! ```
//!
//! ```compile_fail
//! fn twice(permit: kome::RunPermit) -> (kome::RunPermit, kome::RunPermit) { (permit.clone(), permit) }
//! ```
//!
//! 実行権は kome の外では作れない。
//!
//! ```compile_fail
//! fn forge(other: &kome::RunPermit) -> kome::RunPermit { kome::RunPermit { ..*other } }
//! ```
//!
//! 返した実行権は二度使えない。下の二つは二度目の呼び出しの有無だけが違う。
//!
//! ```
//! fn once(kernel: &mut kome::Kernel, permit: kome::RunPermit) {
//!     let zero = kome::Budget::new(0);
//!     let _ = kernel.relinquish(permit, kome::Stop::Trapped, zero);
//! }
//! ```
//!
//! ```compile_fail
//! fn twice(kernel: &mut kome::Kernel, permit: kome::RunPermit) {
//!     let zero = kome::Budget::new(0);
//!     let _ = kernel.relinquish(permit, kome::Stop::Trapped, zero);
//!     let _ = kernel.relinquish(permit, kome::Stop::Trapped, zero);
//! }
//! ```

use crate::error::{Error, Invariant, Refusal, Resource};
use crate::id::{ProcessId, ThreadId};
use crate::kernel::{InstanceId, Kernel, Lent};
use crate::mem::MapGen;
use crate::process::ExitCode;
use crate::thread::Run;

/// 実行量。単位はゲスト命令数など、最適化で変わらないものを実行器が選ぶ。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Budget(u64);

impl Budget {
    /// 数値から作る。
    #[must_use]
    pub const fn new(units: u64) -> Budget {
        Budget(units)
    }

    /// 実行量の数値。
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// ゲスト命令を実行するホスト側の worker の識別。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorkerId(u32);

impl WorkerId {
    /// 数値から作る。
    #[must_use]
    pub const fn new(value: u32) -> WorkerId {
        WorkerId(value)
    }

    /// 識別の数値。
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// 一つのスレッドの何回目の実行か。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RunGen(u64);

impl RunGen {
    pub(crate) const fn new(value: u64) -> RunGen {
        RunGen(value)
    }

    /// 記録と診断のための数値。
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// 一つのスレッドを一度だけ実行する資格。複製できず、kome の外では作れない。
#[must_use = "実行権は Kernel::relinquish で返さなければならない"]
#[derive(Debug, PartialEq, Eq)]
pub struct RunPermit {
    instance: InstanceId,
    thread: ThreadId,
    process: ProcessId,
    worker: WorkerId,
    run: RunGen,
    map: MapGen,
    budget: Budget,
}

impl RunPermit {
    /// 貸した実行インスタンス。
    #[must_use]
    pub const fn instance(&self) -> InstanceId {
        self.instance
    }

    /// 実行するスレッド。
    #[must_use]
    pub const fn thread(&self) -> ThreadId {
        self.thread
    }

    /// スレッドのプロセス (アドレス空間の持ち主)。
    #[must_use]
    pub const fn process(&self) -> ProcessId {
        self.process
    }

    /// 実行を担当する worker。
    #[must_use]
    pub const fn worker(&self) -> WorkerId {
        self.worker
    }

    /// このスレッドの何回目の実行か。
    #[must_use]
    pub const fn run(&self) -> RunGen {
        self.run
    }

    /// 貸した時点のアドレス空間の世代。実行器はこの世代で検証したマッピングだけを使う。
    #[must_use]
    pub const fn map(&self) -> MapGen {
        self.map
    }

    /// 実行してよい量。実行器は区切りごとに検査し、使い切ったら実行権を返す。
    #[must_use]
    pub const fn budget(&self) -> Budget {
        self.budget
    }
}

/// 実行権を返す理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// 実行量を使い切った、または中断の要求に応じた。スレッドは続きを実行できる。
    Preempted,
    /// API 呼び出し、例外、停止命令など、Personality の処理が必要になった。
    Trapped,
}

/// 実行権を返した後のスレッド。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadFate {
    /// 実行待ちへ戻った (停止要求があれば、解除されるまで実行権は貸さない)。
    Ready,
    /// サービス中になった。Personality が処理して [`Kernel::continue_thread`] などを呼ぶ。
    Servicing,
    /// 実行中に受けた終了要求によって終了した。
    Exited(ExitCode),
}

impl Kernel {
    /// 実行可能なスレッドがあれば、先頭のものの実行権を `worker` へ貸す。
    ///
    /// 権限を減らす変更の後、古い世代で実行中のスレッドが残っているプロセスの
    /// スレッドは飛ばす (台帳 MEM-3)。
    pub fn grant(&mut self, worker: WorkerId) -> Result<Option<RunPermit>, Error> {
        if self.running.iter().any(|lent| lent.worker == worker) {
            return Err(Error::Refused(Refusal::WorkerBusy));
        }
        if u64::try_from(self.running.len()).unwrap_or(u64::MAX) >= u64::from(self.limits.workers()) {
            return Err(Error::Exhausted(Resource::Workers));
        }
        let threads = &self.threads;
        let processes = &self.processes;
        let pick = self.ready.position(|thread| {
            threads
                .get(*thread)
                .and_then(|entry| processes.get(entry.process))
                .is_some_and(|process| process.stale_runners == 0)
        });
        let Some(at) = pick else { return Ok(None) };
        let thread = *self.ready.get(at).ok_or(Error::Broken(Invariant::RunQueue))?;
        let entry = self.threads.get(thread).ok_or(Error::Broken(Invariant::RunQueue))?;
        let process = entry.process;
        let run_gen = entry.run_gen.checked_add(1).ok_or(Error::Exhausted(Resource::Identifiers))?;
        let map = self.processes.get(process).ok_or(Error::Broken(Invariant::ProcessTree))?.space.generation();
        // この実行への中断要求の席。
        self.effects.promise(1)?;
        self.ready.remove(at).ok_or(Error::Broken(Invariant::RunQueue))?;
        self.running.push_within(Lent { worker, thread }).map_err(|_| Error::Broken(Invariant::RunOnePermit))?;
        let entry = self.threads.get_mut(thread).ok_or(Error::Broken(Invariant::RunQueue))?;
        entry.run_gen = run_gen;
        entry.run = Run::Running { worker, stale: false, interrupted: false };
        Ok(Some(RunPermit {
            instance: self.instance,
            thread,
            process,
            worker,
            run: RunGen(run_gen),
            map,
            budget: self.limits.quantum(),
        }))
    }

    /// 実行権を受け取り、スレッドの次の状態を決める。
    ///
    /// `consumed` は実際に実行した量。区切りごとの検査なので、貸した量を超えてよい。
    /// 古い実行権、別インスタンスの実行権は状態を変えずに拒否する。
    pub fn relinquish(&mut self, permit: RunPermit, stop: Stop, consumed: Budget) -> Result<ThreadFate, Error> {
        if permit.instance != self.instance {
            return Err(Error::Foreign);
        }
        let thread = permit.thread;
        let stale_permit = Error::Refused(Refusal::StalePermit);
        let entry = self.threads.get_mut(thread).ok_or(stale_permit)?;
        let (stale, interrupted) = match entry.run {
            Run::Running { worker, stale, interrupted }
                if worker == permit.worker && entry.run_gen == permit.run.0 =>
            {
                (stale, interrupted)
            }
            Run::Running { .. } | Run::Ready | Run::Servicing | Run::Blocked(_) => return Err(stale_permit),
        };
        entry.executed = entry.executed.saturating_add(consumed.0);
        entry.run = Run::Servicing;
        let kill = entry.kill;
        let suspended = entry.suspend != 0;
        let process = entry.process;
        if !interrupted {
            self.effects.forgo(1)?;
        }
        self.running
            .remove_first(|lent| lent.thread == thread)
            .ok_or(Error::Broken(Invariant::RunOnePermit))?;
        if stale {
            let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
            owner.stale_runners = owner.stale_runners.checked_sub(1).ok_or(Error::Broken(Invariant::MemBarrier))?;
            if owner.stale_runners == 0 {
                self.flush_limbo(process)?;
            }
        }
        if let Some(code) = kill {
            self.exit_thread_now(thread, code)?;
            return Ok(ThreadFate::Exited(code));
        }
        match stop {
            Stop::Trapped => Ok(ThreadFate::Servicing),
            Stop::Preempted => {
                let entry = self.threads.get_mut(thread).ok_or(Error::Broken(Invariant::RunOnePermit))?;
                entry.run = Run::Ready;
                if !suspended {
                    self.enqueue(thread)?;
                }
                Ok(ThreadFate::Ready)
            }
        }
    }

    /// 実行権を貸している worker の数。
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    /// 実行待ちのスレッドの数。
    #[must_use]
    pub fn ready_count(&self) -> usize {
        self.ready.len()
    }
}
