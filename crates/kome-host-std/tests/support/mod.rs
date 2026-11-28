//! 試験の筋書き: 一つのプロセスの中で、何本かのスレッドが決まった回数だけ譲ってから終了する。
//! 実スレッドの試験と Loom の検査が同じ筋書きを使う。

use kome::{
    AddrWidth, Contexts, Effect, Error, Executor, ExitCode, Instant, Kernel, Limits, PageSize, ProcessId, Ran,
    RunPermit, Start, Stop, Stopped, ThreadFate, ThreadId, InstanceId,
};
use kome_host_std::{Host, Interrupt, Personality};

/// ゲストの文脈: あと何回譲るかと、これまでに実行した回数。
#[derive(Debug)]
pub struct Guest {
    pub left: u64,
    pub steps: u64,
}

/// 文脈を一つ進める実行器。譲る回数を使い切ったら、実行した回数を終了コードにして終了する。
pub struct Stepper;

impl Executor for Stepper {
    type Context = Guest;

    fn run(&mut self, permit: &RunPermit, guest: &mut Guest) -> Stopped {
        guest.steps = guest.steps.saturating_add(1);
        match guest.left.checked_sub(1) {
            Some(left) => {
                guest.left = left;
                Stopped::new(Stop::Yielded, permit.budget())
            }
            None => Stopped::new(Stop::Exited(ExitCode::new(guest.steps)), permit.budget()),
        }
    }
}

/// 終了を記録するだけの Personality。
#[derive(Debug, Default)]
pub struct Watch {
    pub exited: Vec<(ThreadId, u64)>,
    pub process_exited: bool,
    pub yields: u64,
}

impl Personality<Guest> for Watch {
    fn stopped(&mut self, kernel: &mut Kernel, _contexts: &mut Contexts<Guest>, ran: &Ran) -> Result<(), Error> {
        match ran.fate() {
            ThreadFate::Ready => self.yields = self.yields.saturating_add(1),
            ThreadFate::Servicing => kernel.continue_thread(ran.thread())?,
            ThreadFate::Exited(_) => {}
        }
        Ok(())
    }

    fn effect(&mut self, _kernel: &mut Kernel, effect: Effect) -> Result<(), Error> {
        if let Effect::ThreadExited { thread, code, .. } = effect {
            self.exited.push((thread, code.get()));
        }
        if let Effect::ProcessExited { .. } = effect {
            self.process_exited = true;
        }
        Ok(())
    }
}

/// `threads` 本のスレッドが、それぞれ `yields` 回譲ってから終了するホストを作る。
pub fn host(
    workers: u32,
    threads: usize,
    yields: u64,
    clock: impl Fn() -> Instant + Send + Sync + 'static,
) -> (Host<Guest, Watch>, ProcessId, Vec<ThreadId>) {
    let mut kernel = Kernel::new(InstanceId::new(1), Limits::DEFAULT.with_workers(workers)).unwrap();
    let (process, _) = kernel.create_process(AddrWidth::Bits32, PageSize::new(4096).unwrap()).unwrap();
    let mut made = Vec::new();
    for _ in 0..threads {
        let (thread, _) = kernel.create_thread(process, Start::Ready).unwrap();
        made.push(thread);
    }
    let host = Host::new(kernel, Watch::default(), workers, clock);
    host.with(|state| {
        for thread in &made {
            state.contexts.insert(*thread, Guest { left: yields, steps: 0 }).unwrap();
        }
    })
    .unwrap();
    (host, process, made)
}

/// 筋書きが終わった後の状態を確かめる: 全てのスレッドが、決まった回数だけ実行して終了している。
pub fn check(host: &Host<Guest, Watch>, threads: &[ThreadId], yields: u64) {
    host.with(|state| {
        state.kernel.audit().unwrap();
        let mut exited = state.personality.exited.clone();
        exited.sort();
        let mut expected: Vec<(ThreadId, u64)> = threads.iter().map(|thread| (*thread, yields.saturating_add(1))).collect();
        expected.sort();
        assert_eq!(exited, expected);
        assert!(state.personality.process_exited && state.contexts.is_empty());
        assert_eq!(state.personality.yields, yields.saturating_mul(u64::try_from(threads.len()).unwrap()));
        assert_eq!((state.kernel.running_count(), state.kernel.ready_count()), (0, 0));
    })
    .unwrap();
}

/// プロセスが終了したら仕事が終わったと答える Personality。
#[derive(Debug, Default)]
pub struct UntilExit {
    pub watch: Watch,
}

impl Personality<Guest> for UntilExit {
    fn stopped(&mut self, kernel: &mut Kernel, contexts: &mut Contexts<Guest>, ran: &Ran) -> Result<(), Error> {
        self.watch.stopped(kernel, contexts, ran)
    }

    fn effect(&mut self, kernel: &mut Kernel, effect: Effect) -> Result<(), Error> {
        self.watch.effect(kernel, effect)
    }

    fn finished(&self) -> bool {
        self.watch.process_exited
    }
}

/// 仕事が最初から終わっているホスト (スレッドもプロセスも無い)。誰も閉じない。
pub fn finished_host(workers: u32, clock: impl Fn() -> Instant + Send + Sync + 'static) -> Host<Guest, UntilExit> {
    let kernel = Kernel::new(InstanceId::new(1), Limits::DEFAULT.with_workers(workers)).unwrap();
    let personality = UntilExit { watch: Watch { process_exited: true, ..Watch::default() } };
    Host::new(kernel, personality, workers, clock)
}

/// 中断を求められるまでゲストを実行し続ける実行器。
///
/// 試験は、中断を求める直前に `asked` を一つ増やす。止まった回数がそれを超えたら、誰も求めて
/// いない実行が止まった (前の実行への求めを取り違えた) ので、`stale` に数える。
pub struct Spinner {
    pub interrupt: Interrupt,
    pub asked: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub stale: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub stops: u64,
    pub pause: fn(),
}

impl Executor for Spinner {
    type Context = Guest;

    fn run(&mut self, permit: &RunPermit, guest: &mut Guest) -> Stopped {
        guest.steps = guest.steps.saturating_add(1);
        while !self.interrupt.requested(permit) {
            (self.pause)();
        }
        self.stops = self.stops.saturating_add(1);
        if self.stops > self.asked.load(std::sync::atomic::Ordering::SeqCst) {
            self.stale.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Stopped::new(Stop::Interrupted, permit.budget())
    }
}

/// 一つの worker が二本のスレッドを続けて実行する。一本目への中断の求めは、同じ worker が次に
/// 実行する二本目を止めない。`spawn` は worker を別の実スレッドで動かし、終わりを待つ関数を返す。
pub fn interrupt_names_one_run(
    clock: impl Fn() -> Instant + Send + Sync + 'static,
    pause: fn(),
    spawn: impl FnOnce(Box<dyn FnOnce() -> Result<(), kome_host_std::HostFault> + Send>) -> Box<dyn FnOnce() -> Result<(), kome_host_std::HostFault>>,
) {
    let (host, _, made) = host(1, 2, 0, clock);
    let worker = kome::WorkerId::new(0);
    let stale = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let asked = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let ask = |thread| {
        asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        host.with(|state| state.kernel.preempt_thread(thread)).unwrap().unwrap();
    };
    let mut spinner = Spinner {
        interrupt: host.interrupt(worker).unwrap(),
        asked: std::sync::Arc::clone(&asked),
        stale: std::sync::Arc::clone(&stale),
        stops: 0,
        pause,
    };
    let join = {
        let host = host.clone();
        spawn(Box::new(move || host.work(worker, &mut spinner)))
    };
    host.wait_until(|state| state.kernel.running_count() == 1).unwrap();
    ask(made[0]);
    // 一本目が止まり、同じ worker が二本目を実行している。二本目は、自分への求めが来るまで止まらない。
    host.wait_until(|state| state.personality.yields >= 1 && state.kernel.thread_phase(made[1]) == Ok(kome::ThreadPhase::Running)).unwrap();
    host.close().unwrap();
    ask(made[1]);
    assert_eq!(join(), Ok(()));
    assert_eq!(stale.load(std::sync::atomic::Ordering::SeqCst), 0, "前の実行への中断の求めが、次のスレッドの実行を止めた");
    host.with(|state| {
        state.kernel.audit().unwrap();
        assert_eq!(state.personality.yields, 2);
    })
    .unwrap();
}
