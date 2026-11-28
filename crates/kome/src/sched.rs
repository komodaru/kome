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
//! 返した実行権は二度使えない。拒否された実行権だけは、返ってきた値から取り戻せる
//! ([`NotRelinquished`])。下の二つは二度目の呼び出しの有無だけが違う。
//!
//! ```
//! fn once(kernel: &mut kome::Kernel, permit: kome::RunPermit) {
//!     let zero = kome::Budget::new(0);
//!     let _ = kernel.relinquish(permit, kome::Stop::Trap, zero);
//! }
//! ```
//!
//! ```compile_fail
//! fn twice(kernel: &mut kome::Kernel, permit: kome::RunPermit) {
//!     let zero = kome::Budget::new(0);
//!     let _ = kernel.relinquish(permit, kome::Stop::Trap, zero);
//!     let _ = kernel.relinquish(permit, kome::Stop::Trap, zero);
//! }
//! ```

use crate::error::{Error, Invariant, Refusal, Resource};
use crate::id::{ProcessId, ThreadId};
use crate::kernel::{InstanceId, Kernel, Lent};
use crate::mem::{MapGen, MemFault};
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

/// 実行の優先度。値が大きいスレッドへ先に実行権を貸す。同じ値の中では、先に実行待ちに
/// なったものから貸す。
///
/// kome が持つのは、この固定の順序だけである。値をいつ上げ下げするか、実行中のスレッドを
/// いつ止めて譲らせるか ([`Kernel::preempt_thread`]) は Personality が決める。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Priority(u8);

impl Priority {
    /// 作った直後のスレッドの優先度 (中央の値)。
    pub const DEFAULT: Priority = Priority(128);

    /// 数値から作る。
    #[must_use]
    pub const fn new(level: u8) -> Priority {
        Priority(level)
    }

    /// 優先度の数値。
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// 実行器が止まって、実行権を返す理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// 貸した実行量を使い切った。スレッドは実行待ちの末尾へ戻る。
    /// 実行した量が貸した量に届いていなければ拒否する。
    BudgetExhausted,
    /// ゲストが実行を譲った。スレッドは実行待ちの末尾へ戻る。
    Yielded,
    /// 中断の要求 ([`crate::Effect::Interrupt`]) に応じた。スレッドは実行待ちへ戻る
    /// (停止要求があれば貸さず、終了要求があれば終了する)。要求していない実行なら拒否する。
    Interrupted,
    /// API 呼び出し、停止命令、メモリ以外の例外など、Personality の処理が必要になった。
    Trap,
    /// ゲストのメモリアクセスが失敗した。Personality が配送を決める。
    Fault(MemFault),
    /// ゲストがスレッドの終了まで実行した。スレッドはこの終了コードで終了する。
    Exited(ExitCode),
}

/// 実行権を返した後に、スレッドをどうするか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// 実行待ちの末尾へ戻す。
    Requeue,
    /// サービス中にする。
    Service,
    /// 終了させる。
    Exit(ExitCode),
}

/// その理由で実行権を返してよいか。受理する前の検査で、整数だけで決まる。
pub(crate) const fn stop_allowed(stop: Stop, interrupted: bool, lent: Budget, consumed: Budget) -> Result<(), Refusal> {
    match stop {
        Stop::BudgetExhausted if consumed.0 < lent.0 => Err(Refusal::BudgetRemains),
        Stop::Interrupted if !interrupted => Err(Refusal::NotInterrupted),
        Stop::BudgetExhausted | Stop::Yielded | Stop::Interrupted | Stop::Trap | Stop::Fault(_) | Stop::Exited(_) => Ok(()),
    }
}

/// 受理した後のスレッドの行き先。実行中に受けた終了要求 `kill` が、どの理由よりも先に効く。
pub(crate) const fn after_stop(stop: Stop, kill: Option<ExitCode>) -> Next {
    match (kill, stop) {
        (Some(code), _) | (None, Stop::Exited(code)) => Next::Exit(code),
        (None, Stop::Trap | Stop::Fault(_)) => Next::Service,
        (None, Stop::BudgetExhausted | Stop::Yielded | Stop::Interrupted) => Next::Requeue,
    }
}

/// 実行権の返却が成功しなかった理由。受理する前の拒否と、受理した後の障害を区別する。
///
/// 受理する前に拒否した実行権は消費しない。届け先を間違えた実行権は、ここから取り出して
/// 正しい実行インスタンスへ返せる。
#[must_use = "拒否された実行権は into_permit で取り戻す"]
#[derive(Debug, PartialEq, Eq)]
pub enum NotRelinquished {
    /// 受理する前に拒否した。状態は変わっていない。実行権は消費していない。
    Rejected(RunPermit, Error),
    /// 受理して状態を更新する途中で、内部の障害を見つけた。実行権は消費した。
    Failed(Error),
}

impl NotRelinquished {
    /// 理由。
    #[must_use]
    pub const fn error(&self) -> Error {
        match self {
            NotRelinquished::Rejected(_, error) | NotRelinquished::Failed(error) => *error,
        }
    }

    /// 拒否された実行権を取り戻す。受理した後の障害なら、実行権は消費済みで `None`。
    #[must_use]
    pub fn into_permit(self) -> Option<RunPermit> {
        match self {
            NotRelinquished::Rejected(permit, _) => Some(permit),
            NotRelinquished::Failed(_) => None,
        }
    }
}

/// 実行権を返した後のスレッド。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreadFate {
    /// 実行待ちへ戻った (停止要求があれば、解除されるまで実行権は貸さない)。
    Ready,
    /// サービス中になった。Personality が処理して [`Kernel::continue_thread`] などを呼ぶ。
    Servicing,
    /// 終了した。実行中に受けた終了要求によるか、ゲストが終了まで実行した ([`Stop::Exited`])。
    Exited(ExitCode),
}

impl Kernel {
    /// 貸せるスレッドがあれば、優先度が最も高いものの中で、最も先に実行待ちになったものの
    /// 実行権を `worker` へ貸す。
    ///
    /// 権限を減らす変更の後、古い世代で実行中のスレッドが残っているプロセスの
    /// スレッドへは貸さない (台帳 MEM-3)。手数は、実行待ちのスレッドの数の対数で済む。
    pub fn grant(&mut self, worker: WorkerId) -> Result<Option<RunPermit>, Error> {
        self.worker_free(worker)?;
        // 列の先頭が、貸せるスレッドの中で最も先に貸すものである ([`crate::runq`])。
        let at = 0;
        if self.ready.is_empty() {
            return Ok(None);
        }
        self.lend(worker, at).map(Some)
    }

    /// 実行待ちの `thread` を名指しして、実行権を `worker` へ貸す。
    ///
    /// 順序をホストが決める実行 (決まった順序の再現、順序の探索) に使う。優先度と待ち順は
    /// 見ない。貸せる状態でなければ `Refused(NotReady)`、古い世代の実行が残るプロセスなら
    /// `Refused(Barrier)` (台帳 MEM-3)。
    pub fn grant_thread(&mut self, worker: WorkerId, thread: ThreadId) -> Result<RunPermit, Error> {
        let entry = self.threads.find(thread, &self.lineage)?;
        self.worker_free(worker)?;
        if entry.run != Run::Ready || entry.suspend != 0 {
            return Err(Error::Refused(Refusal::NotReady));
        }
        // 貸せる状態で列に無いのは、プロセスに古い世代の実行が残っているときだけである。
        let at = entry.queued.ok_or(Error::Refused(Refusal::Barrier))?;
        self.lend(worker, at)
    }

    /// いま貸せるスレッドの `at` 番目。0 番目は [`Kernel::grant`] が次に貸すスレッドである。
    /// 1 番目から後の並びは、同じ入力列なら同じだが、貸す順でも待った順でもない。
    /// 順序をホストが決めるときに、貸す候補を数えるのに使う。
    #[must_use]
    pub fn ready_thread(&self, at: usize) -> Option<ThreadId> {
        self.ready.get(at).map(|waiting| waiting.thread)
    }

    /// 実行中の `thread` へ中断を要求する (優先度の高いスレッドへ譲らせる、時間切れにする)。
    /// 一度の実行につき、中断の要求は一度だけ出る。実行中でなければ `Refused(NotRunning)`。
    pub fn preempt_thread(&mut self, thread: ThreadId) -> Result<(), Error> {
        match self.threads.find(thread, &self.lineage)?.run {
            Run::Running { .. } => self.interrupt(thread),
            Run::Ready | Run::Servicing | Run::Blocked(_) => Err(Error::Refused(Refusal::NotRunning)),
        }
    }

    /// 優先度を変える。変える前の値を返す。実行待ちになった順番は変えない。
    pub fn set_priority(&mut self, thread: ThreadId, priority: Priority) -> Result<Priority, Error> {
        let entry = self.threads.find_mut(thread, &self.lineage)?;
        let previous = core::mem::replace(&mut entry.priority, priority);
        if let Some(at) = entry.queued {
            self.requeue(at, priority)?;
        }
        Ok(previous)
    }

    /// 優先度。
    pub fn priority(&self, thread: ThreadId) -> Result<Priority, Error> {
        Ok(self.threads.find(thread, &self.lineage)?.priority)
    }

    /// `worker` が実行権を持っておらず、worker の数に空きがあるか。
    fn worker_free(&self, worker: WorkerId) -> Result<(), Error> {
        if self.running.iter().any(|lent| lent.worker == worker) {
            return Err(Error::Refused(Refusal::WorkerBusy));
        }
        if u64::try_from(self.running.len()).unwrap_or(u64::MAX) >= u64::from(self.limits.workers()) {
            return Err(Error::Exhausted(Resource::Workers));
        }
        Ok(())
    }

    /// 実行待ちの列の `at` 番目のスレッドの実行権を貸す。貸す相手を選ぶ二つの入口が共に通る。
    fn lend(&mut self, worker: WorkerId, at: usize) -> Result<RunPermit, Error> {
        let thread = self.ready.get(at).ok_or(Error::Broken(Invariant::RunQueue))?.thread;
        let entry = self.threads.get(thread).ok_or(Error::Broken(Invariant::RunQueue))?;
        let process = entry.process;
        let run_gen = entry.run_gen.checked_add(1).ok_or(Error::Exhausted(Resource::Identifiers))?;
        let map = self.processes.get(process).ok_or(Error::Broken(Invariant::ProcessTree))?.space.generation();
        // 貸し出しの記録は、確保済みの容量へ置く。容量が無いのは内部の障害で、何も変えずに返す。
        if self.running.len() >= self.running.capacity() {
            return Err(Error::Broken(Invariant::RunOnePermit));
        }
        // この実行への中断要求の席。ここから先は失敗しない。
        self.effects.promise(1)?;
        self.unqueue(at)?;
        self.running.push_within(Lent { worker, thread }).map_err(|_| Error::Broken(Invariant::RunOnePermit))?;
        let entry = self.threads.get_mut(thread).ok_or(Error::Broken(Invariant::RunQueue))?;
        entry.run_gen = run_gen;
        entry.run = Run::Running { worker, stale: false, interrupted: false };
        Ok(RunPermit {
            instance: self.lineage.instance(),
            thread,
            process,
            worker,
            run: RunGen(run_gen),
            map,
            budget: self.limits.quantum(),
        })
    }

    /// 実行権を受け取り、スレッドの次の状態を決める。
    ///
    /// `consumed` は実際に実行した量。区切りごとの検査なので、貸した量を超えてよい。
    /// 古い実行権、別インスタンスの実行権、状況と合わない理由 (実行量が残っているのに
    /// [`Stop::BudgetExhausted`]、要求していないのに [`Stop::Interrupted`]) は、状態を変えずに
    /// 拒否し、実行権を [`NotRelinquished::Rejected`] に入れて返す。
    pub fn relinquish(
        &mut self,
        permit: RunPermit,
        stop: Stop,
        consumed: Budget,
    ) -> Result<ThreadFate, NotRelinquished> {
        let admitted = self.admit(&permit).and_then(|(stale, interrupted)| {
            stop_allowed(stop, interrupted, permit.budget, consumed).map_err(Error::Refused)?;
            Ok((stale, interrupted))
        });
        match admitted {
            Ok((stale, interrupted)) => {
                self.take_back(permit.thread, stale, interrupted, stop, consumed).map_err(NotRelinquished::Failed)
            }
            Err(reason) => Err(NotRelinquished::Rejected(permit, reason)),
        }
    }

    /// 実行権が、この状態が貸していて、まだ返っていないものかを確かめる。状態は変えない。
    /// 貸した実行の (古い世代か, 中断を要求済みか) を返す。
    fn admit(&self, permit: &RunPermit) -> Result<(bool, bool), Error> {
        if permit.instance != self.lineage.instance() {
            return Err(Error::Foreign);
        }
        let stale_permit = Error::Refused(Refusal::StalePermit);
        let entry = self.threads.get(permit.thread).ok_or(stale_permit)?;
        match entry.run {
            Run::Running { worker, stale, interrupted }
                if worker == permit.worker && entry.run_gen == permit.run.0 =>
            {
                Ok((stale, interrupted))
            }
            Run::Running { .. } | Run::Ready | Run::Servicing | Run::Blocked(_) => Err(stale_permit),
        }
    }

    /// 受理した実行権のスレッドを、次の状態へ進める。
    fn take_back(
        &mut self,
        thread: ThreadId,
        stale: bool,
        interrupted: bool,
        stop: Stop,
        consumed: Budget,
    ) -> Result<ThreadFate, Error> {
        let entry = self.threads.get_mut(thread).ok_or(Error::Broken(Invariant::RunOnePermit))?;
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
                self.let_in(process)?;
            }
        }
        match after_stop(stop, kill) {
            Next::Exit(code) => {
                self.exit_thread_now(thread, code)?;
                Ok(ThreadFate::Exited(code))
            }
            Next::Service => Ok(ThreadFate::Servicing),
            Next::Requeue => {
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

    /// いま貸せるスレッドの数。実行待ちでも、停止要求があるスレッドと、古い世代の実行が
    /// 残っているプロセスのスレッドは数えない。
    #[must_use]
    pub fn ready_count(&self) -> usize {
        self.ready.len()
    }
}
