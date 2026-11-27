//! kome の参照ホスト。
//!
//! kome の状態 ([`Kernel`]) は一つの書き手しか持てない。このホストは、状態を一つの錠の中に置き、
//! 実スレッドの worker が次の順で動く。
//!
//! 1. 錠を取り、実行権を借り、スレッドの文脈を置き場から取り出す。
//! 2. 錠を放し、ゲストを実行する (ここだけが並行に進む)。
//! 3. 錠を取り、文脈を戻し、止まった理由を添えて実行権を返し、Personality に続きを任せる。
//!
//! ゲストを実行している間は誰も `Kernel` を借りていないので、worker の数だけゲストが同時に進む。
//! 状態を変えるのは常に錠の中の一人で、kome から見れば呼び出しは一列に並ぶ。
//!
//! この順序が、取りこぼした通知や二重の実行を起こさないことは、Loom がスレッドの進み方を
//! 全て試して確かめる (`tests/loom.rs`)。

mod store;
mod sync;

use std::time::Duration;

use kome::{
    Contexts, Effect, Error, Executor, Instant, Kernel, Ran, Refusal, RunPermit, WorkerId, finish_run, start_run,
};

pub use store::{FlatStore, fault};
use sync::{Arc, AtomicU64, Condvar, Mutex, MutexGuard, Ordering};

/// ゲスト OS の側。呼ばれるのは常に錠の中で、`Kernel` を変えられるのはその間だけである。
pub trait Personality<C> {
    /// ゲストが止まった後に呼ばれる。サービス中になったスレッド ([`kome::ThreadFate::Servicing`]) は、
    /// ここで処理して [`Kernel::continue_thread`] などを呼ぶ。
    fn stopped(&mut self, kernel: &mut Kernel, contexts: &mut Contexts<C>, ran: &Ran) -> Result<(), Error>;

    /// kome が外へ頼んだ仕事。実行中の worker への割り込み ([`Effect::Interrupt`]) だけは
    /// ホストが先に処理するので、ここへは来ない。
    fn effect(&mut self, kernel: &mut Kernel, effect: Effect) -> Result<(), Error>;

    /// 仕事が全て終わったか。真を返すと、ホストは自分を閉じ、worker は戻る。
    /// 状態が変わるたびに錠の中で呼ばれる。
    fn finished(&self) -> bool {
        false
    }
}

/// ホストの失敗。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostFault {
    /// kome または Personality が返した失敗。
    Kernel(Error),
    /// 錠を持ったスレッドがパニックした。状態が途中かもしれないので、続けない。
    Poisoned,
    /// その番号の worker は、このホストに無い。
    NoWorker,
}

impl From<Error> for HostFault {
    fn from(error: Error) -> HostFault {
        HostFault::Kernel(error)
    }
}

/// 錠の中にあるもの。
#[derive(Debug)]
pub struct State<C, P> {
    /// kome の状態。
    pub kernel: Kernel,
    /// 実行していないスレッドの文脈。
    pub contexts: Contexts<C>,
    /// ゲスト OS の側。
    pub personality: P,
    closed: bool,
    /// 貸せるスレッドを待って眠っている worker の数。
    idle: u32,
    /// [`Host::wait_until`] で待っている者の数。
    watchers: u32,
}

/// 割り込みが求められていないことを表す値 (実行の世代はここまで進まない)。
const NO_INTERRUPT: u64 = u64::MAX;

/// worker への割り込みの求め。実行器が、ゲストを実行しながら錠を取らずに見る。
#[derive(Clone, Debug)]
pub struct Interrupt {
    wanted: Arc<AtomicU64>,
}

impl Interrupt {
    /// この実行権での実行を止めるよう求められているか。
    ///
    /// 求めは実行の世代を名指しするので、前の実行への求めが次の実行を止めることは無い。
    #[must_use]
    pub fn requested(&self, permit: &RunPermit) -> bool {
        self.wanted.load(Ordering::SeqCst) == permit.run().get()
    }
}

struct Inner<C, P> {
    state: Mutex<State<C, P>>,
    /// 眠っている worker を起こす。
    work: Condvar,
    /// [`Host::wait_until`] で待っている者を起こす。
    watch: Condvar,
    interrupts: Vec<Arc<AtomicU64>>,
    clock: Box<dyn Fn() -> Instant + Send + Sync>,
}

/// 参照ホスト。複製は同じホストを指す。
pub struct Host<C, P> {
    inner: Arc<Inner<C, P>>,
}

impl<C, P> Clone for Host<C, P> {
    fn clone(&self) -> Host<C, P> {
        Host { inner: Arc::clone(&self.inner) }
    }
}

type Guard<'a, C, P> = MutexGuard<'a, State<C, P>>;

impl<C, P: Personality<C>> Host<C, P> {
    /// `workers` 個の worker を持つホストを作る。`clock` は、単調に進む今の時刻を返す。
    pub fn new(
        kernel: Kernel,
        personality: P,
        workers: u32,
        clock: impl Fn() -> Instant + Send + Sync + 'static,
    ) -> Host<C, P> {
        let state = State { kernel, contexts: Contexts::new(), personality, closed: false, idle: 0, watchers: 0 };
        let interrupts = (0..workers).map(|_| Arc::new(AtomicU64::new(NO_INTERRUPT))).collect();
        Host { inner: Arc::new(Inner { state: Mutex::new(state), work: Condvar::new(), watch: Condvar::new(), interrupts, clock: Box::new(clock) }) }
    }

    fn lock(&self) -> Result<Guard<'_, C, P>, HostFault> {
        self.inner.state.lock().map_err(|_| HostFault::Poisoned)
    }

    /// worker への割り込みの求めを見る口。実行器に持たせる。
    pub fn interrupt(&self, worker: WorkerId) -> Result<Interrupt, HostFault> {
        let wanted = usize::try_from(worker.get()).ok().and_then(|at| self.inner.interrupts.get(at));
        Ok(Interrupt { wanted: Arc::clone(wanted.ok_or(HostFault::NoWorker)?) })
    }

    /// 時刻を進め、kome が外へ頼んだ仕事を全て処理する。錠の中で、状態を変えた後に必ず呼ぶ。
    /// 期限が来たか、頼まれた仕事があったかを返す (あれば、待っている者を起こす)。
    fn settle(&self, state: &mut State<C, P>) -> Result<bool, HostFault> {
        let now = (self.inner.clock)();
        let mut changed = false;
        while now > state.kernel.now() {
            let advanced = state.kernel.advance(now)?;
            changed |= advanced.fired() != 0;
            if !advanced.more() {
                break;
            }
        }
        while let Some(effect) = state.kernel.poll_effect() {
            changed = true;
            if let Effect::Interrupt { worker, run, .. } = effect {
                let wanted = usize::try_from(worker.get()).ok().and_then(|at| self.inner.interrupts.get(at));
                wanted.ok_or(HostFault::NoWorker)?.store(run.get(), Ordering::SeqCst);
                continue;
            }
            if let Effect::ThreadExited { thread, .. } = effect {
                // 実行していない間に終了したスレッドの文脈を捨てる。実行して終了したものは
                // finish_run が既に取り除いているので、無いのは誤りではない。
                match state.contexts.remove(thread) {
                    Ok(_) | Err(Error::Refused(Refusal::NoContext)) => {}
                    Err(error) => return Err(HostFault::Kernel(error)),
                }
            }
            state.personality.effect(&mut state.kernel, effect)?;
        }
        if !state.closed && state.personality.finished() {
            state.closed = true;
            changed = true;
        }
        Ok(changed)
    }

    /// 状態を変えた後に、起きる理由のある者だけを起こす。`before` は変える前の最も近い期限。
    ///
    /// 眠っている worker が起きる理由は三つある: 閉じた、貸せるスレッドがあるかもしれない、
    /// 待つべき期限が変わった。数は錠の中で数えているので、待っている者がいなければ何も呼ばない。
    fn wake(&self, state: &State<C, P>, before: Option<Instant>) {
        if state.watchers != 0 {
            self.inner.watch.notify_all();
        }
        let reason = state.closed || state.kernel.ready_count() != 0 || state.kernel.next_deadline() != before;
        if state.idle != 0 && reason {
            self.inner.work.notify_all();
        }
    }

    /// 錠の中で状態を使う。終わったら、頼まれた仕事を処理し、待っている全員を起こす。
    ///
    /// プロセスやスレッドを作る、I/O の完了を伝える、といった外からの操作は全てここを通る。
    pub fn with<R>(&self, act: impl FnOnce(&mut State<C, P>) -> R) -> Result<R, HostFault> {
        let mut guard = self.lock()?;
        let before = guard.kernel.next_deadline();
        let result = act(&mut guard);
        let settled = self.settle(&mut guard);
        self.wake(&guard, before);
        drop(guard);
        settled.map(|_| result)
    }

    /// `done` が真を返すまで待つ。`done` は錠の中で、状態が変わるたびに呼ばれる。
    pub fn wait_until(&self, mut done: impl FnMut(&State<C, P>) -> bool) -> Result<(), HostFault> {
        let mut guard = self.lock()?;
        while !done(&guard) && !guard.closed {
            guard.watchers = guard.watchers.saturating_add(1);
            guard = self.inner.watch.wait(guard).map_err(|_| HostFault::Poisoned)?;
            guard.watchers = guard.watchers.saturating_sub(1);
        }
        Ok(())
    }

    /// 全ての worker に終わるよう伝える。実行中のゲストは、止まったところで終わる。
    pub fn close(&self) -> Result<(), HostFault> {
        self.lock()?.closed = true;
        self.wake_everyone();
        Ok(())
    }

    /// 閉じたときと失敗したときに、待っている全員を起こす。
    fn wake_everyone(&self) {
        self.inner.work.notify_all();
        self.inner.watch.notify_all();
    }

    /// 起こされるか、最も近い期限が来るまで眠る。
    fn sleep<'a>(&self, mut guard: Guard<'a, C, P>) -> Result<Guard<'a, C, P>, HostFault> {
        guard.idle = guard.idle.saturating_add(1);
        let woken = match guard.kernel.next_deadline() {
            Some(deadline) => {
                let left = deadline.nanos().saturating_sub((self.inner.clock)().nanos());
                let waited = self.inner.work.wait_timeout(guard, Duration::from_nanos(left));
                waited.map(|(guard, _)| guard).map_err(|_| HostFault::Poisoned)
            }
            None => self.inner.work.wait(guard).map_err(|_| HostFault::Poisoned),
        };
        let mut guard = woken?;
        guard.idle = guard.idle.saturating_sub(1);
        Ok(guard)
    }

    /// worker として働く。[`Host::close`] が呼ばれるまで戻らない。
    ///
    /// 失敗で戻るときは、ほかの worker も終わるようにホストを閉じる。
    pub fn work<E: Executor<Context = C>>(&self, worker: WorkerId, executor: &mut E) -> Result<(), HostFault> {
        let result = self.work_until_closed(worker, executor);
        if result.is_err() {
            // 錠が壊れていても、待っている全員を起こす。
            if let Ok(mut guard) = self.inner.state.lock() {
                guard.closed = true;
            }
            self.wake_everyone();
        }
        result
    }

    fn work_until_closed<E: Executor<Context = C>>(&self, worker: WorkerId, executor: &mut E) -> Result<(), HostFault> {
        let mut guard = self.lock()?;
        loop {
            let (permit, mut loaded) = loop {
                if guard.closed {
                    return Ok(());
                }
                let before = guard.kernel.next_deadline();
                let settled = self.settle(&mut guard)?;
                let state = &mut *guard;
                if let Some(permit) = state.kernel.grant(worker)? {
                    // 実行中になったことも状態の変化なので、待っている者に知らせる。
                    self.wake(state, before);
                    break start_run(&mut state.kernel, permit, &mut state.contexts)?;
                }
                if settled {
                    self.wake(state, before);
                }
                guard = self.sleep(guard)?;
            };
            drop(guard);
            let stopped = executor.run(&permit, loaded.context());
            guard = self.lock()?;
            let before = guard.kernel.next_deadline();
            let state = &mut *guard;
            let ran = finish_run(&mut state.kernel, permit, loaded, stopped, &mut state.contexts)?;
            state.personality.stopped(&mut state.kernel, &mut state.contexts, &ran)?;
            self.settle(&mut guard)?;
            // `loom_broken` は、この通知を抜いた誤りを Loom が行き詰まりとして見つけることを
            // 確かめるためだけにある (`scripts/ci/loom.sh`)。
            #[cfg(not(loom_broken))]
            self.wake(&guard, before);
        }
    }
}
