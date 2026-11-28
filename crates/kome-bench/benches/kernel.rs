//! よく通る操作の一回あたりの時間。`cargo bench -p kome-bench`、絞り込みは `-- 名前の一部`。

use std::thread;

use kome::{
    Access, AddrWidth, BackingOffset, Budget, Contexts, Deadline, Effect, Error, Executor, ExitCode, GuestAddr,
    GuestRange, InstanceId, Instant, Interruptible, Kernel, Limits, MapSource, PageSize, ProcessId, Prot, Ran,
    RunPermit, SignalMode, SignalOp, SignalSpec, Start, Stop, Stopped, ThreadFate, ThreadId, WaitMode, WaitStart,
    WorkerId, guest_read, run_once,
};
use kome_bench::{Bench, must, some};
use kome_host_std::{FlatStore, Host, Personality};

const PAGE: u64 = 4096;
const W0: WorkerId = WorkerId::new(0);

fn kernel() -> (Kernel, ProcessId) {
    let limits = Limits::DEFAULT.with_threads(2048).with_workers(4);
    let mut kernel = must(Kernel::new(InstanceId::new(1), limits));
    let (process, _) = must(kernel.create_process(AddrWidth::Bits64, some(PageSize::new(PAGE))));
    (kernel, process)
}

fn range(page: u64, pages: u64) -> GuestRange {
    some(GuestRange::new(GuestAddr::new(page.saturating_mul(PAGE)), pages.saturating_mul(PAGE)))
}

/// 何もせずに譲る実行器。
struct Yielder;

impl Executor for Yielder {
    type Context = u64;

    fn run(&mut self, permit: &RunPermit, context: &mut u64) -> Stopped {
        *context = context.wrapping_add(1);
        Stopped::new(Stop::Yielded, permit.budget())
    }
}

fn scheduling(bench: &Bench) {
    let (mut k, p) = kernel();
    must(k.create_thread(p, Start::Ready));
    bench.measure("実行権を借りて返す", || {
        let permit = some(must(k.grant(W0)));
        must(k.relinquish(permit, Stop::Yielded, Budget::new(0)).map_err(|refused| refused.error()))
    });
    // 実行待ちが多くても、貸す相手は列の先頭から選ぶ。
    for waiting in [16u64, 1024] {
        let (mut k, p) = kernel();
        for _ in 0..waiting {
            must(k.create_thread(p, Start::Ready));
        }
        bench.measure(&format!("実行権を借りて返す (実行待ち {waiting} 本)"), || {
            let permit = some(must(k.grant(W0)));
            must(k.relinquish(permit, Stop::Yielded, Budget::new(0)).map_err(|refused| refused.error()))
        });
    }
    for threads in [1u64, 64, 1024] {
        let (mut k, p) = kernel();
        let mut contexts = Contexts::new();
        for _ in 0..threads {
            let (thread, _) = must(k.create_thread(p, Start::Ready));
            must(contexts.insert(thread, 0));
        }
        bench.measure(&format!("文脈つきで一回実行する (スレッド {threads} 本)"), || {
            some(must(run_once(&mut k, W0, &mut Yielder, &mut contexts)))
        });
    }
}

fn waiting(bench: &Bench) {
    let (mut k, p) = kernel();
    let (thread, _) = must(k.create_thread(p, Start::Ready));
    let object = must(k.create_object(p, SignalSpec::new(SignalMode::Consume, 0, 1)));
    let permit = some(must(k.grant(W0)));
    must(k.relinquish(permit, Stop::Trap, Budget::new(0)).map_err(|refused| refused.error()));
    bench.measure("待機を始めて通知で起こす", || {
        let started = must(k.wait(thread, &[object], WaitMode::Any, Deadline::Never, Interruptible::No));
        assert_eq!(started, WaitStart::Blocked);
        must(k.signal(object, SignalOp::Add(1)));
        some(k.poll_effect())
    });
}

fn channels(bench: &Bench) {
    let (mut k, p) = kernel();
    let made = must(k.create_channel(p, kome::ChannelSpec::messages(16, 256)));
    let (message, mut buffer) = ([7u8; 64], [0u8; 256]);
    bench.measure("64 バイトのメッセージを送って受け取る", || {
        must(k.send(p, made.channel(), &message, None));
        let received = must(k.receive(p, made.channel(), &mut buffer));
        while k.poll_effect().is_some() {}
        received
    });
}

fn memory(bench: &Bench) {
    let (mut k, p) = kernel();
    let backing = must(k.create_backing(p, 256 * PAGE));
    // 隣り合う範囲がまとまらないように、権限を交互に変える。
    for page in 0..128u64 {
        let prot = if page % 2 == 0 { Prot::READ } else { Prot::READ_WRITE };
        let source = MapSource::Backing { backing, offset: BackingOffset::new(page.saturating_mul(PAGE)) };
        must(k.map(p, range(page.saturating_add(16), 1), source, prot));
    }
    let wanted = range(16 + 77, 1);
    bench.measure("アドレスを引く (マッピング 128 個)", || must(must(k.translate(p, wanted, Access::Read))));
    let source = MapSource::Backing { backing, offset: BackingOffset::new(0) };
    let fresh = range(1024, 4);
    bench.measure("マッピングを置いて外す (マッピング 128 個)", || {
        must(k.map(p, fresh, source, Prot::READ_WRITE));
        must(k.unmap(p, fresh))
    });
    let mut store = FlatStore::new();
    must(store.create(backing, 256 * PAGE));
    let mut buffer = [0u8; 4096];
    let start = GuestAddr::new((16 + 76) * PAGE + 100);
    bench.measure("ゲストのアドレスで 4096 バイト読む (二つのマッピングをまたぐ)", || {
        must(guest_read(&k, &store, p, start, &mut buffer))
    });
}

fn timers(bench: &Bench) {
    let (mut k, p) = kernel();
    let object = must(k.create_object(p, SignalSpec::new(SignalMode::Keep, 0, u64::MAX)));
    let timer = must(k.create_timer(p, object));
    let mut now = 0u64;
    bench.measure("タイマを仕掛けて期限まで進める", || {
        now = now.saturating_add(1);
        must(k.arm_timer(timer, Instant::from_nanos(now), None));
        let advanced = must(k.advance(Instant::from_nanos(now)));
        assert!(!advanced.more() && advanced.fired() == 1);
        while k.poll_effect().is_some() {}
        must(k.signal(object, SignalOp::Set(0)))
    });
}

fn snapshots(bench: &Bench) {
    let (mut k, p) = kernel();
    for _ in 0..64 {
        must(k.create_thread(p, Start::Ready));
    }
    let mut instance = 1u64;
    bench.measure("写しを取って復元する (スレッド 64 本)", || {
        instance = instance.saturating_add(1);
        must(must(k.snapshot()).restore(InstanceId::new(instance)))
    });
}

/// あと何回譲るかを数える実行器。使い切ったら終了する。
struct Countdown;

impl Executor for Countdown {
    type Context = u64;

    fn run(&mut self, permit: &RunPermit, left: &mut u64) -> Stopped {
        match left.checked_sub(1) {
            Some(rest) => {
                *left = rest;
                Stopped::new(Stop::Yielded, permit.budget())
            }
            None => Stopped::new(Stop::Exited(ExitCode::new(0)), permit.budget()),
        }
    }
}

/// プロセスの終了だけを見る Personality。
#[derive(Default)]
struct Quiet {
    done: bool,
}

impl Personality<u64> for Quiet {
    fn stopped(&mut self, kernel: &mut Kernel, _contexts: &mut Contexts<u64>, ran: &Ran) -> Result<(), Error> {
        match ran.fate() {
            ThreadFate::Servicing => kernel.continue_thread(ran.thread()),
            ThreadFate::Ready | ThreadFate::Exited(_) => Ok(()),
        }
    }

    fn effect(&mut self, _kernel: &mut Kernel, effect: Effect) -> Result<(), Error> {
        if let Effect::ProcessExited { .. } = effect {
            self.done = true;
        }
        Ok(())
    }

    fn finished(&self) -> bool {
        self.done
    }
}

/// `workers` 個の worker が、`threads` 本のスレッドを合わせて `yields` 回譲らせて終了させる。
fn host_run(workers: u32, threads: u64, yields: u64) {
    let (mut k, p) = kernel();
    let made: Vec<ThreadId> = (0..threads).map(|_| must(k.create_thread(p, Start::Ready)).0).collect();
    let host: Host<u64, Quiet> = Host::new(k, Quiet::default(), workers, || Instant::from_nanos(0));
    must(host.with(|state| {
        for thread in &made {
            must(state.contexts.insert(*thread, yields / threads));
        }
    }));
    thread::scope(|scope| {
        for worker in 0..workers {
            let host = host.clone();
            scope.spawn(move || must(host.work(WorkerId::new(worker), &mut Countdown)));
        }
    });
    must(host.with(|state| assert!(state.personality.done && state.contexts.is_empty())));
}

fn hosts(bench: &Bench) {
    for (workers, threads) in [(1u32, 1u64), (4, 4), (4, 64)] {
        let name = format!("参照ホストで一回譲る (worker {workers} 個、スレッド {threads} 本)");
        bench.measure_whole(&name, 200_000, |yields| host_run(workers, threads, yields.max(threads)));
    }
}

fn main() {
    let bench = Bench::from_args();
    scheduling(&bench);
    waiting(&bench);
    channels(&bench);
    memory(&bench);
    timers(&bench);
    snapshots(&bench);
    hosts(&bench);
    if let Err(why) = bench.finish() {
        eprintln!("kome-bench: {why}");
        std::process::exit(1);
    }
}
