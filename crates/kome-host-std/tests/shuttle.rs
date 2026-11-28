//! 並列実行の決定的シミュレーション。参照ホストの worker を、種から決まる順序で動かす。
//!
//! `scripts/ci/shuttle.sh`
//!
//! Loom (`tests/loom.rs`) は小さな筋書きの進み方を全て試す。こちらは、Loom では大きすぎる筋書きを、
//! 種から決まる進み方で数多く試す。筋書き (スレッドごとの台本、外からの操作) も同じ種から作るので、
//! 落ちた回は、Shuttle が出す種から一歩ずつ同じに再現できる (`a_failing_seed_replays`)。
//!
//! 時刻は仮想で、進めるのは `ticker` だけである。Shuttle の `wait_timeout` は時間では起きないので、
//! 時刻を進めた者がホストに知らせる (`Host::with`)。実際の時計で眠りから起きる経路は、実スレッドの
//! 試験 (`tests/host.rs`) が見る。
#![cfg(shuttle)]

use kome::{
    AddrWidth, Contexts, Deadline, Effect, Error, Executor, ExitCode, InstanceId, Instant, Interruptible, Kernel,
    Limits, ObjectId, PageSize, Ran, RunPermit, SignalMode, SignalOp, SignalSpec, Start, Stop, Stopped, ThreadFate,
    ThreadId, ThreadPhase, WaitMode, WaitOutcome, WaitStart, WorkerId,
};
use kome_host_std::{Host, HostFault, Interrupt, Personality, State};
use shuttle::rand::{Rng, thread_rng};
use shuttle::scheduler::RandomScheduler;
use shuttle::sync::Arc;
use shuttle::sync::atomic::{AtomicU64, Ordering};
use shuttle::thread;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const WORKERS: u32 = 3;
const THREADS: usize = 5;
/// 一本のスレッドの台本の長さの上限。
const STEPS: usize = 6;
/// 外からの操作の数。
const MEDDLES: usize = 6;
/// 外から終了させたスレッドの終了コード (台本の長さからは出ない値)。
const KILLED: u64 = 999_999;

/// ゲストが一回の実行ですること。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// 譲る。
    Yield,
    /// 実行量を使い切る。
    Budget,
    /// 中断を求められるまで実行し続ける。
    Spin,
    /// Personality を呼ぶ。
    Call(Call),
}

/// Personality への呼び出し。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Call {
    /// この長さだけ眠る (決して成立しない資源を、期限つきで待つ)。
    Nap(u64),
    /// 門が開くまで待つ (中断を受け付ける)。
    Enter,
    /// 門を開ける。
    Open,
}

/// ゲストの文脈: 台本と、今の位置と、実行した回数。
#[derive(Debug)]
struct Actor {
    script: Vec<Step>,
    at: usize,
    runs: u64,
}

/// 台本を一つ進める実行器。台本が尽きたら、実行した回数を終了コードにして終了する。
struct Player {
    interrupt: Interrupt,
}

impl Executor for Player {
    type Context = Actor;

    fn run(&mut self, permit: &RunPermit, actor: &mut Actor) -> Stopped {
        actor.runs += 1;
        let step = actor.script.get(actor.at).copied();
        actor.at += 1;
        let stop = match step {
            None => Stop::Exited(ExitCode::new(actor.runs)),
            Some(Step::Yield) => Stop::Yielded,
            Some(Step::Budget) => Stop::BudgetExhausted,
            Some(Step::Call(_)) => Stop::Trap,
            Some(Step::Spin) => {
                while !self.interrupt.requested(permit) {
                    thread::yield_now();
                }
                Stop::Interrupted
            }
        };
        Stopped::new(stop, permit.budget())
    }
}

/// 台本の呼び出しを処理する Personality。起きたことを順に記録する。
#[derive(Debug)]
struct Town {
    /// 開くと、待っている全員が通る門。
    gate: ObjectId,
    /// 決して成立しない資源。
    never: ObjectId,
    /// スレッドごとの、これから来る呼び出し。
    calls: BTreeMap<ThreadId, VecDeque<Call>>,
    exited: Vec<(ThreadId, u64)>,
    process_exited: bool,
    /// スレッドごとの、止まった回数。
    stops: BTreeMap<ThreadId, u64>,
    /// 錠の中で起きたことの記録。同じ種なら、一字も違わない。
    trace: Vec<String>,
    /// 通った種類の出来事。
    seen: BTreeSet<&'static str>,
}

impl Town {
    fn serve(&mut self, kernel: &mut Kernel, thread: ThreadId) -> Result<(), Error> {
        let call = self.calls.get_mut(&thread).and_then(VecDeque::pop_front);
        let started = match call {
            Some(Call::Nap(nanos)) => {
                let due = Instant::from_nanos(kernel.now().nanos().saturating_add(nanos));
                kernel.wait(thread, &[self.never], WaitMode::Any, Deadline::At(due), Interruptible::No)?
            }
            Some(Call::Enter) => kernel.wait(thread, &[self.gate], WaitMode::Any, Deadline::Never, Interruptible::Yes)?,
            Some(Call::Open) => {
                kernel.signal(self.gate, SignalOp::Set(1))?;
                self.seen.insert("call:open");
                return kernel.continue_thread(thread);
            }
            None => return Err(Error::Refused(kome::Refusal::NotServicing)),
        };
        match started {
            WaitStart::Blocked => {
                self.seen.insert("wait:blocked");
                Ok(())
            }
            WaitStart::Done(_) => {
                self.seen.insert("wait:at-once");
                kernel.continue_thread(thread)
            }
        }
    }
}

impl Personality<Actor> for Town {
    fn stopped(&mut self, kernel: &mut Kernel, _contexts: &mut Contexts<Actor>, ran: &Ran) -> Result<(), Error> {
        self.trace.push(format!("{:?} {:?}", ran.thread(), ran.fate()));
        *self.stops.entry(ran.thread()).or_default() += 1;
        match ran.fate() {
            ThreadFate::Ready => {
                self.seen.insert("stop:ready");
                Ok(())
            }
            ThreadFate::Servicing => self.serve(kernel, ran.thread()),
            ThreadFate::Exited(code) => {
                if code.get() == KILLED {
                    self.seen.insert("stop:killed-while-running");
                }
                Ok(())
            }
        }
    }

    fn effect(&mut self, kernel: &mut Kernel, effect: Effect) -> Result<(), Error> {
        self.trace.push(format!("{effect:?}"));
        match effect {
            Effect::WaitDone { thread, outcome } => {
                self.seen.insert(match outcome {
                    WaitOutcome::TimedOut => "wait:timed-out",
                    WaitOutcome::Signaled { .. } => "wait:signaled",
                    WaitOutcome::AllSignaled | WaitOutcome::Interrupted => "wait:other",
                });
                kernel.continue_thread(thread)
            }
            Effect::ThreadExited { thread, code, .. } => {
                self.exited.push((thread, code.get()));
                Ok(())
            }
            Effect::ProcessExited { .. } => {
                self.process_exited = true;
                Ok(())
            }
            Effect::Interrupt { .. }
            | Effect::ProcessReclaimed { .. }
            | Effect::ObjectReclaimed { .. }
            | Effect::BackingReclaimed { .. }
            | Effect::IoSubmit { .. }
            | Effect::IoCancel { .. } => Ok(()),
        }
    }

    fn finished(&self) -> bool {
        self.process_exited
    }
}

/// 外からの操作。相手は、スレッドの列での位置。
#[derive(Clone, Copy, Debug)]
enum Meddle {
    /// 止めて、少し後で再開する。
    Pause(usize),
    /// 実行中なら譲らせる。
    Poke(usize),
    /// 門で待っていれば、待機を中断して先へ進ませる。
    Nudge(usize),
    /// 終了させる。
    Kill(usize),
    /// 門を開ける。
    Open,
}

/// 外からの操作の結果。拒否と古い ID は、相手の状態しだいで起きる。それ以外の失敗は誤りである。
fn landed(result: Result<Result<(), Error>, HostFault>) -> bool {
    match result.unwrap() {
        Ok(()) => true,
        Err(Error::Refused(_) | Error::Stale(_)) => false,
        Err(other) => panic!("外からの操作が、拒否ではない失敗で終わった: {other:?}"),
    }
}

fn meddle(host: &Host<Actor, Town>, threads: &[ThreadId], gate: ObjectId, plan: &[Meddle]) -> BTreeSet<&'static str> {
    let mut seen = BTreeSet::new();
    for act in plan {
        thread::yield_now();
        match *act {
            Meddle::Pause(at) => {
                if landed(host.with(|state| state.kernel.suspend_thread(threads[at]).map(|_| ()))) {
                    seen.insert("meddle:pause");
                    thread::yield_now();
                    landed(host.with(|state| state.kernel.unsuspend_thread(threads[at]).map(|_| ())));
                }
            }
            Meddle::Poke(at) => {
                if landed(host.with(|state| state.kernel.preempt_thread(threads[at]))) {
                    seen.insert("meddle:poke");
                }
            }
            Meddle::Nudge(at) => {
                let nudged = host.with(|state| {
                    state.kernel.interrupt_wait(threads[at])?;
                    state.kernel.continue_thread(threads[at])
                });
                if landed(nudged) {
                    seen.insert("meddle:nudge");
                }
            }
            Meddle::Kill(at) => {
                if landed(host.with(|state| state.kernel.terminate_thread(threads[at], ExitCode::new(KILLED)))) {
                    seen.insert("meddle:kill");
                }
            }
            Meddle::Open => {
                landed(host.with(|state| state.kernel.signal(gate, SignalOp::Set(1)).map(|_| ())));
            }
        }
    }
    // 最後に必ず門を開ける。門で待つスレッドが、開ける者のいないまま残らないように。
    landed(host.with(|state| state.kernel.signal(gate, SignalOp::Set(1)).map(|_| ())));
    seen
}

/// 仮想の時刻を進める。誰かが期限を待っている間だけ、その期限までのどこかへ進めて、ホストに知らせる。
fn tick(host: &Host<Actor, Town>, clock: &AtomicU64) {
    loop {
        host.wait_until(|state| state.personality.process_exited || state.kernel.next_deadline().is_some()).unwrap();
        let (exited, due, now) =
            host.with(|state| (state.personality.process_exited, state.kernel.next_deadline(), state.kernel.now())).unwrap();
        if exited {
            return;
        }
        if let Some(due) = due {
            let gap = due.nanos().saturating_sub(now.nanos()).max(1);
            // 半分は期限ちょうどまで、半分は期限の手前のどこかまで進める。
            let step = if thread_rng().gen_bool(0.5) { gap } else { thread_rng().gen_range(1..=gap) };
            clock.fetch_max(now.nanos() + step, Ordering::SeqCst);
            host.with(|_| ()).unwrap();
        }
    }
}

/// 実行中で、今の実行にまだ中断を求めていないスレッドと、そのスレッドがこれまでに止まった回数。
fn unasked(state: &State<Actor, Town>, threads: &[ThreadId], asked: &BTreeMap<ThreadId, u64>) -> Vec<(ThreadId, u64)> {
    let stops = |thread: &ThreadId| state.personality.stops.get(thread).copied().unwrap_or(0);
    let running = threads.iter().filter(|thread| state.kernel.thread_phase(**thread) == Ok(ThreadPhase::Running));
    running.map(|thread| (*thread, stops(thread))).filter(|(thread, stops)| asked.get(thread) != Some(stops)).collect()
}

/// 始まった実行の全てに、一度ずつ中断を求める。中断を待って実行し続けるスレッド (`Step::Spin`) は、
/// これで止まる。実行が始まったことは、ホストの通知だけで知る (求めた後は、次の通知まで眠る)。
fn ward(host: &Host<Actor, Town>, threads: &[ThreadId]) {
    let mut asked = BTreeMap::new();
    loop {
        host.wait_until(|state| state.personality.process_exited || !unasked(state, threads, &asked).is_empty()).unwrap();
        let exited = host
            .with(|state| {
                for (thread, stops) in unasked(state, threads, &asked) {
                    landed(Ok(state.kernel.preempt_thread(thread)));
                    asked.insert(thread, stops);
                }
                state.personality.process_exited
            })
            .unwrap();
        if exited {
            return;
        }
    }
}

/// 一回の筋書きの結果: 錠の中で起きたことの記録と、通った種類の出来事。
struct Told {
    trace: Vec<String>,
    seen: BTreeSet<&'static str>,
}

/// 筋書き: `WORKERS` 個の worker が `THREADS` 本のスレッドを台本どおりに実行する間に、時刻が進み、
/// 外からの操作 (停止と再開、譲らせる、待機の中断、終了、門を開ける) が割り込む。
///
/// どの進み方でも、行き詰まらず、全てのスレッドが終了し、外から終了させなかったスレッドは台本の
/// 長さちょうどの回数だけ実行して終了し、検査を通る。
fn storm() -> Told {
    let mut rng = thread_rng();
    let mut kernel = Kernel::new(InstanceId::new(1), Limits::DEFAULT.with_workers(WORKERS)).unwrap();
    let (process, _) = kernel.create_process(AddrWidth::Bits32, PageSize::new(4096).unwrap()).unwrap();
    let gate = kernel.create_object(process, SignalSpec::new(SignalMode::Keep, 0, 1)).unwrap();
    let never = kernel.create_object(process, SignalSpec::new(SignalMode::Keep, 0, 1)).unwrap();
    let mut threads = Vec::new();
    let mut scripts = Vec::new();
    let mut calls = BTreeMap::new();
    for _ in 0..THREADS {
        let (thread, _) = kernel.create_thread(process, Start::Ready).unwrap();
        let script: Vec<Step> = (0..rng.gen_range(0..=STEPS))
            .map(|_| match rng.gen_range(0..7u32) {
                0 | 1 => Step::Yield,
                2 => Step::Budget,
                3 => Step::Spin,
                4 => Step::Call(Call::Nap(rng.gen_range(1..=50))),
                5 => Step::Call(Call::Enter),
                _ => Step::Call(Call::Open),
            })
            .collect();
        let wanted: VecDeque<Call> = script.iter().filter_map(|step| if let Step::Call(call) = step { Some(*call) } else { None }).collect();
        calls.insert(thread, wanted);
        threads.push(thread);
        scripts.push(script);
    }
    let plan: Vec<Meddle> = (0..MEDDLES)
        .map(|_| {
            let at = rng.gen_range(0..THREADS);
            match rng.gen_range(0..6u32) {
                0 | 1 => Meddle::Pause(at),
                2 => Meddle::Poke(at),
                3 => Meddle::Nudge(at),
                4 => Meddle::Kill(at),
                _ => Meddle::Open,
            }
        })
        .collect();

    let clock = Arc::new(AtomicU64::new(0));
    let town = Town { gate, never, calls, exited: Vec::new(), process_exited: false, stops: BTreeMap::new(), trace: Vec::new(), seen: BTreeSet::new() };
    let host = {
        let clock = Arc::clone(&clock);
        Host::new(kernel, town, WORKERS, move || Instant::from_nanos(clock.load(Ordering::SeqCst)))
    };
    host.with(|state| {
        for (thread, script) in threads.iter().zip(&scripts) {
            state.contexts.insert(*thread, Actor { script: script.clone(), at: 0, runs: 0 }).unwrap();
        }
    })
    .unwrap();

    let workers: Vec<_> = (0..WORKERS)
        .map(|worker| {
            let host = host.clone();
            let mut player = Player { interrupt: host.interrupt(WorkerId::new(worker)).unwrap() };
            thread::spawn(move || host.work(WorkerId::new(worker), &mut player))
        })
        .collect();
    let ticker = {
        let (host, clock) = (host.clone(), Arc::clone(&clock));
        thread::spawn(move || tick(&host, &clock))
    };
    let warden = {
        let (host, threads) = (host.clone(), threads.clone());
        thread::spawn(move || ward(&host, &threads))
    };
    let meddler = {
        let (host, threads) = (host.clone(), threads.clone());
        thread::spawn(move || meddle(&host, &threads, gate, &plan))
    };

    host.wait_until(|state| state.personality.process_exited).unwrap();
    let mut seen = meddler.join().unwrap();
    // 誰もホストを閉じない。仕事が終わったことは Personality が答え、worker は自分で戻る。
    for worker in workers {
        assert_eq!(worker.join().unwrap(), Ok(()));
    }
    ticker.join().unwrap();
    warden.join().unwrap();

    let trace = host
        .with(|state| {
            state.kernel.audit().unwrap();
            assert!(state.personality.process_exited && state.contexts.is_empty());
            assert_eq!((state.kernel.running_count(), state.kernel.ready_count()), (0, 0));
            let mut exited = state.personality.exited.clone();
            exited.sort();
            assert_eq!(exited.len(), THREADS, "終了していないスレッドがある、または二度終了した");
            for ((thread, script), (ended, code)) in threads.iter().zip(&scripts).zip(&exited) {
                assert_eq!(thread, ended);
                let whole = u64::try_from(script.len()).unwrap() + 1;
                // 外から終了させたスレッドは、台本の途中で終わる。それ以外は、台本を一歩も飛ばさず、
                // 一歩も繰り返さずに終わる。
                assert!(*code == whole || *code == KILLED, "{thread:?} は {whole} 回の実行で終わるはずが、終了コードは {code}");
            }
            seen.extend(state.personality.seen.iter().copied());
            state.personality.trace.clone()
        })
        .unwrap();
    Told { trace, seen }
}

/// 筋書きを、種から決まる `iterations` 通りの進み方で走らせる。回ごとの記録を返す。
fn told(seed: u64, iterations: usize) -> Vec<Told> {
    let runs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&runs);
    shuttle::Runner::new(RandomScheduler::new_from_seed(seed, iterations), shuttle::Config::new()).run(move || {
        let run = storm();
        sink.lock().unwrap().push(run);
    });
    std::sync::Arc::try_unwrap(runs).ok().unwrap().into_inner().unwrap()
}

/// 試す進み方の数。
const SCHEDULES: usize = 50_000;

#[test]
fn any_schedule_of_the_storm_runs_every_thread_to_its_end() {
    let runs = told(0x6b6f_6d65, SCHEDULES);
    assert_eq!(runs.len(), SCHEDULES);
    // 筋書きが、確かめたい出来事の全てを通っている (台本や操作が当たらずに終わっていない)。
    let seen: BTreeSet<&str> = runs.iter().flat_map(|run| run.seen.iter().copied()).collect();
    let all = [
        "call:open",
        "meddle:kill",
        "meddle:nudge",
        "meddle:pause",
        "meddle:poke",
        "stop:killed-while-running",
        "stop:ready",
        "wait:at-once",
        "wait:blocked",
        "wait:signaled",
        "wait:timed-out",
    ];
    assert_eq!(seen.into_iter().collect::<Vec<_>>(), all);
    println!("shuttle: {SCHEDULES} 通りの進み方で、worker {WORKERS} 個とスレッド {THREADS} 本の筋書きを確かめた");
}

/// 同じ種は、同じ台本と同じ進み方を選ぶ。錠の中で起きたことの記録が、一字も違わない。
/// 種を変えれば記録も変わる (進み方を実際に変えている)。
#[test]
fn the_same_seed_gives_the_same_run() {
    let mut distinct = BTreeSet::new();
    for seed in [1, 2, 3] {
        let first: Vec<Vec<String>> = told(seed, 40).into_iter().map(|run| run.trace).collect();
        let again: Vec<Vec<String>> = told(seed, 40).into_iter().map(|run| run.trace).collect();
        assert_eq!(first, again, "種 {seed} の二度の実行で、記録が違う");
        distinct.extend(first);
    }
    assert!(distinct.len() > 100, "進み方がほとんど変わっていない: 120 回で {} 通り", distinct.len());
    println!("shuttle: 同じ種の二度の実行で記録が一致した (120 回のうち {} 通りの記録)", distinct.len());
}

/// 落ちた回を再現する。Shuttle が落ちたときに出す種 (failing seed) を `KOME_SHUTTLE_SEED` に入れると、
/// その回の台本と進み方だけを、もう一度たどる。
#[test]
fn a_failing_seed_replays() {
    let Some(seed) = std::env::var("KOME_SHUTTLE_SEED").ok().map(|seed| seed.trim().parse().unwrap()) else {
        return;
    };
    assert_eq!(told(seed, 1).len(), 1);
}
