//! 試験の筋書き: 一つのプロセスの中で、何本かのスレッドが決まった回数だけ譲ってから終了する。
//! 実スレッドの試験と Loom の検査が同じ筋書きを使う。

use kome::{
    AddrWidth, Contexts, Effect, Error, Executor, ExitCode, Instant, Kernel, Limits, PageSize, ProcessId, Ran,
    RunPermit, Start, Stop, Stopped, ThreadFate, ThreadId, InstanceId,
};
use kome_host_std::{Host, Personality};

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
