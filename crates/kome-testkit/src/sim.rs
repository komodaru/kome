//! バイト列で駆動する決定的シミュレーションと、履歴の監視。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

use kome::{
    Access, AddrWidth, BackingId, BackingOffset, Budget, CancelReply, Completion, CompletionKind, Deadline, Duration,
    Effect, Error, ExitCode, GuestAddr, GuestRange, HostError, InstanceId, Instant, Interruptible, IoId, IoLinks,
    IoOutcome, IoPhase, Kernel, Limits, MapSource, MemFaultKind, ObjectId, PageSize, Period, ProcessId, ProcessPhase, Prot,
    Refusal, RunPermit, SignalMode, SignalOp, SignalSpec, Start, Stop, Tag, ThreadId, ThreadPhase, TimerId, WaitMode, WaitOutcome,
    WaitStart, WorkerId,
};

use crate::pages::{Expect, Pages};

/// シミュレーションのページの大きさ。
pub const PAGE: u64 = 0x1000;
/// モデルと比べるページ数。操作はこの範囲にだけマッピングを置く。
const PAGES: u64 = 24;
const WORKERS: u32 = 3;

/// 上限に当たりやすい小さな構成。
#[must_use]
pub fn limits() -> Limits {
    Limits::DEFAULT
        .with_processes(3)
        .with_threads(6)
        .with_objects(24)
        .with_backings(5)
        .with_backing_bytes(12 * PAGE)
        .with_timers(4)
        .with_ios(6)
        .with_workers(2)
        .with_effects(48)
        .with_mappings(6)
        .with_wait_targets(3)
        .with_holdings(8)
        .with_quantum(Budget::new(10))
}

/// 確かめた性質が破れた。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// 何番目の操作で破れたか。
    pub step: usize,
    /// 何が破れたか。
    pub message: String,
}

/// 一回の実行の結果。同じバイト列からは同じ値になる。
#[derive(Debug, PartialEq, Eq)]
pub struct Summary {
    /// 解釈した操作の数。
    pub steps: usize,
    /// 受理された操作の数。
    pub accepted: usize,
    /// 拒否された操作の数。
    pub refused: usize,
    /// 出た `Effect` のすべて。
    pub effects: Vec<Effect>,
    /// 最後の状態。
    pub kernel: Kernel,
    /// 写しから別の実行インスタンスとして復元し、そちらへ乗り換えた回数。
    pub restores: u64,
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl Reader<'_> {
    fn done(&self) -> bool {
        self.bytes.is_empty()
    }

    /// 次のバイト。尽きたら 0。
    fn byte(&mut self) -> u8 {
        match self.bytes.split_first() {
            Some((first, rest)) => {
                self.bytes = rest;
                *first
            }
            None => 0,
        }
    }

    fn below(&mut self, bound: u64) -> u64 {
        u64::from(self.byte()).checked_rem(bound).unwrap_or(0)
    }

    fn flag(&mut self) -> bool {
        self.byte() & 1 == 1
    }

    /// 最近作ったものを選びやすくして、一覧から一つ選ぶ。解放済みの ID も選ばれる。
    fn pick<T: Copy>(&mut self, pool: &[T]) -> Option<T> {
        let byte = self.byte();
        Reader::nth_recent(pool, byte)
    }

    /// 8 回に 7 回は条件に合うものから選ぶ (操作が受理されやすい相手)。残りと、合うものが
    /// 無いときは [`Reader::pick`] と同じで、状態に合わない相手や解放済みの ID も選ぶ。
    fn prefer<T: Copy>(&mut self, pool: &[T], fits: impl Fn(&T) -> bool) -> Option<T> {
        let byte = self.byte();
        if byte < 224 {
            let fitting: Vec<T> = pool.iter().copied().filter(|item| fits(item)).collect();
            if let Some(found) = Reader::nth_recent(&fitting, byte) {
                return Some(found);
            }
        }
        Reader::nth_recent(pool, byte)
    }

    fn nth_recent<T: Copy>(pool: &[T], byte: u8) -> Option<T> {
        let window = pool.len().min(6);
        let back = usize::from(byte).checked_rem(window)?;
        pool.len().checked_sub(1)?.checked_sub(back).and_then(|at| pool.get(at)).copied()
    }
}

struct Held {
    permit: RunPermit,
    /// 貸した時点で、そのプロセスのアドレス空間が見せていた実体。
    backings: BTreeSet<BackingId>,
    /// 貸した後に、そのプロセスで権限を減らす変更があったか。
    stale: bool,
}

/// 操作の結果: 受理、拒否、または選ぶ相手が無くて何もしなかった。
enum Outcome {
    Accepted,
    Refused(Error),
    Skipped,
}

/// 確保しないと約束している操作 (docs/memory.md) の前後で呼ぶ印。
///
/// 試験基盤そのものは確保を数えない (大域のアロケータを差し替えるには unsafe が要る)。
/// `checks/noalloc` がこの印の間の確保を数え、0 であることを確かめる。
#[derive(Clone, Copy, Debug)]
pub struct Probe {
    /// 操作を呼ぶ直前。
    pub enter: fn(),
    /// 操作から戻った直後。
    pub leave: fn(),
}

impl Probe {
    /// 何もしない印。
    pub const NONE: Probe = Probe { enter: || {}, leave: || {} };
}

struct Sim {
    k: Kernel,
    probe: Probe,
    processes: Vec<ProcessId>,
    threads: Vec<ThreadId>,
    objects: Vec<ObjectId>,
    backings: Vec<BackingId>,
    timers: Vec<TimerId>,
    ios: Vec<IoId>,
    held: Vec<Held>,
    /// 登録中の待機と、その対象。
    blocked: BTreeMap<ThreadId, Vec<ObjectId>>,
    /// 増減を数えている「消費する資源」と、あるはずの数。タイマや I/O の通知先にしたものは外す。
    counted: BTreeMap<ObjectId, u64>,
    settled: BTreeSet<IoId>,
    submitted: BTreeSet<IoId>,
    cancel_sent: BTreeSet<IoId>,
    /// ホストへ出て、まだ応答していない I/O。
    in_flight: Vec<IoId>,
    gone_processes: BTreeSet<ProcessId>,
    gone_threads: BTreeSet<ThreadId>,
    gone_objects: BTreeSet<ObjectId>,
    gone_backings: BTreeSet<BackingId>,
    spaces: BTreeMap<ProcessId, Pages>,
    effects: Vec<Effect>,
    accepted: usize,
    refused: usize,
    /// 復元した回数。復元のたびに新しい実行インスタンスの識別を作る。
    restores: u64,
}

fn fresh<T: Ord + Copy + Debug>(gone: &BTreeSet<T>, id: T) -> Result<(), String> {
    if gone.contains(&id) { Err(format!("ID-1: 回収した ID {id:?} が新しい資源を指した")) } else { Ok(()) }
}

fn retire<T: Ord + Copy + Debug>(gone: &mut BTreeSet<T>, id: T) -> Result<(), String> {
    if gone.insert(id) { Ok(()) } else { Err(format!("{id:?} を二度回収した")) }
}

fn done(result: Result<(), Error>) -> Outcome {
    match result {
        Ok(()) => Outcome::Accepted,
        Err(error) => Outcome::Refused(error),
    }
}

fn pages(first: u64, count: u64) -> Option<GuestRange> {
    GuestRange::new(GuestAddr::new(first.checked_mul(PAGE)?), count.checked_mul(PAGE)?)
}

impl Sim {
    /// 確保しないと約束している操作を、印で挟んで呼ぶ。
    fn quiet<T>(&mut self, call: impl FnOnce(&mut Kernel) -> T) -> T {
        (self.probe.enter)();
        let out = call(&mut self.k);
        (self.probe.leave)();
        out
    }

    fn new(probe: Probe) -> Result<Sim, String> {
        let k = Kernel::new(InstanceId::new(1), limits()).map_err(|error| format!("{error:?}"))?;
        Ok(Sim {
            k,
            probe,
            processes: Vec::new(),
            threads: Vec::new(),
            objects: Vec::new(),
            backings: Vec::new(),
            timers: Vec::new(),
            ios: Vec::new(),
            held: Vec::new(),
            blocked: BTreeMap::new(),
            counted: BTreeMap::new(),
            settled: BTreeSet::new(),
            submitted: BTreeSet::new(),
            cancel_sent: BTreeSet::new(),
            in_flight: Vec::new(),
            gone_processes: BTreeSet::new(),
            gone_threads: BTreeSet::new(),
            gone_objects: BTreeSet::new(),
            gone_backings: BTreeSet::new(),
            spaces: BTreeMap::new(),
            effects: Vec::new(),
            accepted: 0,
            refused: 0,
            restores: 0,
        })
    }

    fn alive(&self) -> impl Fn(&ProcessId) -> bool + '_ {
        |process| self.k.process_phase(*process) == Ok(ProcessPhase::Alive)
    }

    fn in_phase(&self, phase: ThreadPhase) -> impl Fn(&ThreadId) -> bool + '_ {
        move |thread| self.k.thread_phase(*thread) == Ok(phase)
    }

    fn exists(&self) -> impl Fn(&ObjectId) -> bool + '_ {
        |object| self.k.signal_count(*object).is_ok()
    }

    fn io_in(&self, phases: &'static [IoPhase]) -> impl Fn(&IoId) -> bool + '_ {
        move |io| self.k.io_phase(*io).is_ok_and(|phase| phases.contains(&phase))
    }

    /// 成立した待機が、数えている資源を消費したことを記録する。
    fn consumed(&mut self, targets: &[ObjectId], outcome: WaitOutcome) -> Result<(), String> {
        let chosen: &[ObjectId] = match outcome {
            WaitOutcome::Signaled { index } => {
                let at = usize::try_from(index).map_err(|error| error.to_string())?;
                targets.get(at..=at).ok_or("WAIT-1: 成立した対象の位置が対象の列の外にある")?
            }
            WaitOutcome::AllSignaled => targets,
            WaitOutcome::TimedOut | WaitOutcome::Interrupted => &[],
        };
        for target in chosen {
            if let Some(count) = self.counted.get_mut(target) {
                *count = count.checked_sub(1).ok_or(format!("数が 0 の {target:?} で待機が成立した"))?;
            }
        }
        Ok(())
    }

    fn on_effect(&mut self, effect: Effect) -> Result<(), String> {
        match effect {
            Effect::Interrupt { worker, thread, run } => {
                let known = self
                    .held
                    .iter()
                    .any(|held| held.permit.worker() == worker && held.permit.thread() == thread && held.permit.run() == run);
                if !known {
                    return Err(format!("貸していない実行への中断: {effect:?}"));
                }
            }
            Effect::WaitDone { thread, outcome } => {
                let targets =
                    self.blocked.remove(&thread).ok_or(format!("WAIT-1: 登録の無い待機が確定した: {effect:?}"))?;
                if outcome == WaitOutcome::Interrupted {
                    return Err("WAIT-1: 中断の結果が Effect で届いた".into());
                }
                self.consumed(&targets, outcome)?;
            }
            Effect::ThreadExited { thread, .. } => {
                retire(&mut self.gone_threads, thread)?;
                self.blocked.remove(&thread);
                if self.held.iter().any(|held| held.permit.thread() == thread) {
                    return Err(format!("RUN-1: 実行権を貸したままのスレッドが終了した: {thread:?}"));
                }
                if self.k.thread_phase(thread) != Err(Error::Stale(Tag::Thread)) {
                    return Err(format!("ID-1: 終了したスレッドの ID がまだ生きている: {thread:?}"));
                }
            }
            Effect::ProcessExited { process, .. } => {
                self.spaces.remove(&process);
                let phase = self.k.process_phase(process);
                if !matches!(phase, Ok(ProcessPhase::Draining) | Err(Error::Stale(Tag::Process))) {
                    return Err(format!("PROC-1: 終了を通知したプロセスが {phase:?}"));
                }
            }
            Effect::ProcessReclaimed { process } => {
                retire(&mut self.gone_processes, process)?;
                if self.k.process_phase(process) != Err(Error::Stale(Tag::Process)) {
                    return Err(format!("ID-1: 回収したプロセスの ID がまだ生きている: {process:?}"));
                }
            }
            Effect::ObjectReclaimed { object } => {
                retire(&mut self.gone_objects, object)?;
                self.counted.remove(&object);
                // 終了したスレッドの待機は、終了の通知より先に外れている。
                let waits = |thread: &ThreadId| self.k.thread_phase(*thread).is_ok();
                if self.blocked.iter().any(|(thread, targets)| waits(thread) && targets.contains(&object)) {
                    return Err(format!("OBJ-1: 待機の対象を破棄した: {object:?}"));
                }
                if self.k.signal_count(object) != Err(Error::Stale(Tag::Object)) {
                    return Err(format!("ID-1: 破棄した資源の ID がまだ生きている: {object:?}"));
                }
            }
            Effect::BackingReclaimed { backing } => {
                retire(&mut self.gone_backings, backing)?;
                if self.held.iter().any(|held| held.stale && held.backings.contains(&backing)) {
                    return Err(format!("MEM-1: 古い世代の実行が使っている実体を解放した: {backing:?}"));
                }
                // 終了処理の中の解放は、終了の通知より先に届く。資源を手放し終えたプロセスのモデルは見ない。
                let alive = |process: &ProcessId| {
                    matches!(self.k.process_phase(*process), Ok(ProcessPhase::Alive | ProcessPhase::Exiting))
                };
                if self.spaces.iter().any(|(process, space)| alive(process) && space.shows(backing)) {
                    return Err(format!("MEM-1: マッピングが残る実体を解放した: {backing:?}"));
                }
            }
            Effect::IoSubmit { io } => {
                if !self.submitted.insert(io) {
                    return Err(format!("FX-1: I/O を二度開始した: {io:?}"));
                }
                self.in_flight.push(io);
            }
            Effect::IoCancel { io } => {
                if !self.submitted.contains(&io) || !self.cancel_sent.insert(io) {
                    return Err(format!("FX-1: 開始していない I/O の取消、または二度目の取消: {io:?}"));
                }
            }
        }
        Ok(())
    }

    /// 操作の前に `seen` 個あったキューの、それより後ろを監視へ渡す。キューは取り出さない。
    fn observe(&mut self, seen: usize) -> Result<(), String> {
        let mut at = seen;
        while let Some(effect) = self.k.peek_effect(at) {
            self.on_effect(effect)?;
            self.effects.push(effect);
            at = at.saturating_add(1);
        }
        Ok(())
    }

    fn check_counts(&self) -> Result<(), String> {
        for (object, expected) in &self.counted {
            let actual = self.k.signal_count(*object);
            if actual != Ok(*expected) {
                return Err(format!("{object:?} の数が {actual:?}、増減の合計は {expected}"));
            }
        }
        Ok(())
    }

    /// モデルとカーネルで、すべてのページの見え方を比べる。
    fn check_space(&self, process: ProcessId) -> Result<(), String> {
        let Some(model) = self.spaces.get(&process) else { return Ok(()) };
        if self.k.process_phase(process).is_err() {
            return Ok(());
        }
        for page in 0..PAGES.saturating_add(4) {
            for access in [Access::Read, Access::Write, Access::Execute] {
                let within = 0x10;
                let addr = page.saturating_mul(PAGE).saturating_add(within);
                let range = GuestRange::new(GuestAddr::new(addr), 1).ok_or("範囲")?;
                let actual = match self.k.translate(process, range, access).map_err(|error| format!("{error:?}"))? {
                    Ok(span) if span.bytes() == 1 => Expect::Reaches(span.backing(), span.offset().get()),
                    Ok(span) => return Err(format!("1 バイトの要求に {} バイトの区間", span.bytes())),
                    Err(fault) if fault.addr() != GuestAddr::new(addr) => {
                        return Err(format!("失敗のアドレスが {:?}、要求は {addr:#x}", fault.addr()));
                    }
                    Err(fault) => match fault.kind() {
                        MemFaultKind::Unmapped => Expect::Unmapped,
                        MemFaultKind::Reserved => Expect::Reserved,
                        MemFaultKind::Denied => Expect::Denied,
                    },
                };
                let expected = model.expect(page, within, access);
                if actual != expected {
                    return Err(format!("ページ {page} の {access:?}: カーネル {actual:?}、モデル {expected:?}"));
                }
            }
        }
        Ok(())
    }

    fn outcome(byte: u8) -> IoOutcome {
        let transferred = u64::from(byte >> 2);
        match byte & 3 {
            0 | 1 => IoOutcome::Done { transferred },
            2 => IoOutcome::Failed { error: HostError::new(5), transferred },
            _ => IoOutcome::Cancelled { transferred },
        }
    }

    fn op(&mut self, code: u8, r: &mut Reader<'_>) -> Result<Outcome, String> {
        let page = PageSize::new(PAGE).ok_or("ページの大きさ")?;
        Ok(match code {
            0 => {
                let width = if r.flag() { AddrWidth::Bits32 } else { AddrWidth::Bits64 };
                match self.k.create_process(width, page) {
                    Ok((process, exit)) => {
                        fresh(&self.gone_processes, process)?;
                        fresh(&self.gone_objects, exit)?;
                        self.processes.push(process);
                        self.objects.push(exit);
                        self.spaces.insert(process, Pages::default());
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            1 => {
                if r.below(6) != 0 {
                    return Ok(Outcome::Skipped);
                }
                let Some(process) = r.prefer(&self.processes, self.alive()) else { return Ok(Outcome::Skipped) };
                done(self.quiet(|k| k.exit_process(process, ExitCode::new(u64::from(r.byte())))))
            }
            2 => {
                let Some(process) = r.prefer(&self.processes, self.alive()) else { return Ok(Outcome::Skipped) };
                let start = if r.below(4) == 0 { Start::Suspended } else { Start::Ready };
                match self.k.create_thread(process, start) {
                    Ok((thread, exit)) => {
                        fresh(&self.gone_threads, thread)?;
                        fresh(&self.gone_objects, exit)?;
                        self.threads.push(thread);
                        self.objects.push(exit);
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            3 => {
                let Some(thread) = r.prefer(&self.threads, self.in_phase(ThreadPhase::Servicing)) else { return Ok(Outcome::Skipped) };
                done(self.quiet(|k| k.continue_thread(thread)))
            }
            4 => {
                let Some(thread) = r.pick(&self.threads) else { return Ok(Outcome::Skipped) };
                done(self.quiet(|k| k.suspend_thread(thread)).map(|_| ()))
            }
            5 => {
                let Some(thread) = r.prefer(&self.threads, |thread| self.k.suspend_count(*thread).is_ok_and(|count| count > 0)) else { return Ok(Outcome::Skipped) };
                done(self.quiet(|k| k.unsuspend_thread(thread)).map(|_| ()))
            }
            6 => {
                if r.below(3) != 0 {
                    return Ok(Outcome::Skipped);
                }
                let Some(thread) = r.pick(&self.threads) else { return Ok(Outcome::Skipped) };
                done(self.quiet(|k| k.terminate_thread(thread, ExitCode::new(u64::from(r.byte())))))
            }
            7 | 8 => {
                let worker = WorkerId::new(u32::try_from(r.below(u64::from(WORKERS))).map_err(|e| e.to_string())?);
                match self.quiet(|k| k.grant(worker)) {
                    Ok(Some(permit)) => {
                        let process = permit.process();
                        for held in &self.held {
                            if held.permit.thread() == permit.thread() || held.permit.worker() == permit.worker() {
                                return Err(format!("RUN-1: 実行権が重複した: {permit:?}"));
                            }
                            if held.permit.process() == process && held.stale {
                                return Err(format!("MEM-3: 古い世代の実行が残るプロセスへ貸した: {permit:?}"));
                            }
                        }
                        if permit.instance() != self.k.instance() || Ok(permit.map()) != self.k.map_generation(process) {
                            return Err(format!("実行権の内容が状態と一致しない: {permit:?}"));
                        }
                        let backings = self.spaces.get(&process).map(|space| space.backings().collect()).unwrap_or_default();
                        self.held.push(Held { permit, backings, stale: false });
                        Outcome::Accepted
                    }
                    Ok(None) => Outcome::Accepted,
                    Err(error) => Outcome::Refused(error),
                }
            }
            9 | 10 => {
                if self.held.is_empty() {
                    return Ok(Outcome::Skipped);
                }
                let at = usize::try_from(r.below(u64::try_from(self.held.len()).map_err(|e| e.to_string())?))
                    .map_err(|e| e.to_string())?;
                let held = self.held.remove(at);
                let stop = if r.flag() { Stop::Trapped } else { Stop::Preempted };
                let consumed = Budget::new(u64::from(r.byte()));
                match self.quiet(|k| k.relinquish(held.permit, stop, consumed)) {
                    Ok(_) => Outcome::Accepted,
                    Err(error) => return Err(format!("RUN-1: 正当な実行権が拒否された: {error:?}")),
                }
            }
            11 => {
                let Some(process) = r.prefer(&self.processes, self.alive()) else { return Ok(Outcome::Skipped) };
                let (mode, initial, max) = match r.below(4) {
                    0 => (SignalMode::Keep, 0, 1),
                    1 => (SignalMode::Keep, 1, 1),
                    2 => (SignalMode::Consume, 0, 1),
                    _ => (SignalMode::Consume, r.below(3), 3),
                };
                match self.k.create_object(process, SignalSpec::new(mode, initial, max)) {
                    Ok(object) => {
                        fresh(&self.gone_objects, object)?;
                        self.objects.push(object);
                        if mode == SignalMode::Consume {
                            self.counted.insert(object, initial);
                        }
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            12 => {
                let (Some(process), Some(object)) = (r.prefer(&self.processes, self.alive()), r.prefer(&self.objects, self.exists())) else {
                    return Ok(Outcome::Skipped);
                };
                done(self.k.retain_object(process, object))
            }
            13 => {
                let (Some(process), Some(object)) = (r.prefer(&self.processes, self.alive()), r.prefer(&self.objects, self.exists())) else {
                    return Ok(Outcome::Skipped);
                };
                done(self.quiet(|k| k.release_object(process, object)))
            }
            14 | 15 => {
                let Some(object) = r.prefer(&self.objects, self.exists()) else { return Ok(Outcome::Skipped) };
                let op = if r.flag() { SignalOp::Add(r.below(3)) } else { SignalOp::Set(r.below(4)) };
                match self.quiet(|k| k.signal(object, op)) {
                    Ok(previous) => {
                        if let Some(count) = self.counted.get_mut(&object) {
                            *count = match op {
                                SignalOp::Add(amount) => previous.saturating_add(amount),
                                SignalOp::Set(value) => value,
                            };
                        }
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            16..=18 => {
                let Some(thread) = r.prefer(&self.threads, self.in_phase(ThreadPhase::Servicing)) else { return Ok(Outcome::Skipped) };
                let mut targets = Vec::new();
                for _ in 0..r.below(4) {
                    targets.extend(r.prefer(&self.objects, self.exists()));
                }
                let mode = if r.flag() { WaitMode::All } else { WaitMode::Any };
                let now = self.k.now().nanos();
                let deadline = match r.below(4) {
                    0 => Deadline::Never,
                    1 => Deadline::At(Instant::from_nanos(now)),
                    _ => Deadline::At(Instant::from_nanos(now.saturating_add(r.below(5).saturating_mul(40)))),
                };
                let interruptible = if r.flag() { Interruptible::Yes } else { Interruptible::No };
                match self.k.wait(thread, &targets, mode, deadline, interruptible) {
                    Ok(WaitStart::Done(outcome)) => {
                        self.consumed(&targets, outcome)?;
                        Outcome::Accepted
                    }
                    Ok(WaitStart::Blocked) => {
                        if self.blocked.insert(thread, targets).is_some() {
                            return Err(format!("WAIT-1: 待機中のスレッドがもう一つ待機を登録した: {thread:?}"));
                        }
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            19 => {
                let Some(thread) = r.prefer(&self.threads, self.in_phase(ThreadPhase::Blocked)) else { return Ok(Outcome::Skipped) };
                match self.quiet(|k| k.interrupt_wait(thread)) {
                    Ok(()) => {
                        self.blocked.remove(&thread).ok_or("WAIT-1: 登録の無い待機を中断した")?;
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            20 => {
                let (Some(process), Some(object)) = (r.prefer(&self.processes, self.alive()), r.prefer(&self.objects, self.exists())) else {
                    return Ok(Outcome::Skipped);
                };
                match self.k.create_timer(process, object) {
                    Ok(timer) => {
                        self.timers.push(timer);
                        self.counted.remove(&object);
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            21 => {
                let Some(timer) = r.pick(&self.timers) else { return Ok(Outcome::Skipped) };
                let at = Instant::from_nanos(self.k.now().nanos().saturating_add(r.below(4).saturating_mul(50)));
                let min = limits().min_period();
                let period = match r.below(3) {
                    0 => None,
                    1 => Some(min),
                    _ => Period::new(Duration::from_nanos(min.get().nanos().saturating_sub(1))),
                };
                done(self.k.arm_timer(timer, at, period))
            }
            22 => {
                let Some(timer) = r.pick(&self.timers) else { return Ok(Outcome::Skipped) };
                done(self.quiet(|k| k.disarm_timer(timer)).map(|_| ()))
            }
            23 => {
                let Some(timer) = r.pick(&self.timers) else { return Ok(Outcome::Skipped) };
                done(self.quiet(|k| k.drop_timer(timer)))
            }
            24..=26 => {
                let now = self.k.now().nanos();
                let to = match (r.flag(), self.k.next_deadline()) {
                    (true, Some(next)) => next.max(self.k.now()),
                    (true, None) | (false, _) => Instant::from_nanos(now.saturating_add(r.below(8).saturating_mul(35))),
                };
                done(self.quiet(|k| k.advance(to)).map(|_| ()))
            }
            27 | 28 => {
                let Some(process) = r.prefer(&self.processes, self.alive()) else { return Ok(Outcome::Skipped) };
                let mut links = IoLinks::NONE;
                let (target, notify) = match r.below(4) {
                    0 => (None, None),
                    1 => (r.prefer(&self.objects, self.exists()), None),
                    2 => (None, r.prefer(&self.objects, self.exists())),
                    _ => {
                        let both = r.prefer(&self.objects, self.exists());
                        (both, if r.flag() { both } else { r.prefer(&self.objects, self.exists()) })
                    }
                };
                if let Some(target) = target {
                    links = links.with_target(target);
                }
                if let Some(notify) = notify {
                    links = links.with_notify(notify);
                }
                match self.k.prepare_io(process, links) {
                    Ok(io) => {
                        self.ios.push(io);
                        if let Some(notify) = notify {
                            self.counted.remove(&notify);
                        }
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            29 | 30 => {
                let Some(io) = r.prefer(&self.ios, self.io_in(&[IoPhase::Prepared])) else { return Ok(Outcome::Skipped) };
                done(self.quiet(|k| k.issue_io(io)))
            }
            31 => {
                let Some(io) = r.prefer(&self.ios, self.io_in(&[IoPhase::Issued, IoPhase::Prepared])) else { return Ok(Outcome::Skipped) };
                match self.quiet(|k| k.cancel_io(io)) {
                    Ok(CancelReply::Withdrawn) => {
                        if !self.settled.insert(io) {
                            return Err(format!("IO-1: 終端済みの I/O を取り下げた: {io:?}"));
                        }
                        Outcome::Accepted
                    }
                    Ok(CancelReply::Requested | CancelReply::AlreadyRequested) => Outcome::Accepted,
                    Err(error) => Outcome::Refused(error),
                }
            }
            32..=34 => {
                // ホストの応答。応答待ちのものを選ぶか、任意の I/O へ二度目・遅い通知を送る。
                let io = if r.flag() { r.pick(&self.in_flight) } else { r.pick(&self.ios) };
                let Some(io) = io else { return Ok(Outcome::Skipped) };
                let outcome = Sim::outcome(r.byte());
                let awaited = self.in_flight.contains(&io);
                let instance = if r.below(16) == 0 { InstanceId::new(2) } else { self.k.instance() };
                match self.quiet(|k| k.complete(Completion::new(instance, CompletionKind::Io { io, outcome }))) {
                    Ok(_) => {
                        if !awaited || instance != self.k.instance() {
                            return Err(format!("IO-1: 応答を待っていない I/O の完了を受理した: {io:?}"));
                        }
                        if !self.settled.insert(io) {
                            return Err(format!("IO-1: 終端済みの I/O が再度終端した: {io:?}"));
                        }
                        self.in_flight.retain(|pending| *pending != io);
                        Outcome::Accepted
                    }
                    Err(error) => {
                        if awaited && instance == self.k.instance() {
                            return Err(format!("応答待ちの I/O の完了が拒否された: {io:?} {error:?}"));
                        }
                        Outcome::Refused(error)
                    }
                }
            }
            35 | 36 => {
                let Some(io) = r.prefer(&self.ios, self.io_in(&[IoPhase::Settled])) else { return Ok(Outcome::Skipped) };
                let before = self.k.io_outcome(io);
                match self.quiet(|k| k.publish_io(io)) {
                    Ok(outcome) => {
                        if before != Ok(Some(outcome)) {
                            return Err(format!("IO-1: 公開した結果 {outcome:?} が確定した結果 {before:?} と違う"));
                        }
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            37 => {
                let Some(io) = r.prefer(&self.ios, self.io_in(&[IoPhase::Published, IoPhase::Settled])) else { return Ok(Outcome::Skipped) };
                let phase = self.k.io_phase(io);
                match self.quiet(|k| k.release_io(io)) {
                    Ok(()) => {
                        if matches!(phase, Ok(IoPhase::Issued | IoPhase::CancelRequested)) {
                            return Err(format!("応答待ちの I/O を回収した: {io:?}"));
                        }
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            38 => {
                let Some(process) = r.prefer(&self.processes, self.alive()) else { return Ok(Outcome::Skipped) };
                match self.k.create_backing(process, r.below(4).saturating_add(1).saturating_mul(PAGE)) {
                    Ok(backing) => {
                        fresh(&self.gone_backings, backing)?;
                        self.backings.push(backing);
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            39 => {
                let (Some(process), Some(backing)) = (r.prefer(&self.processes, self.alive()), r.prefer(&self.backings, |backing| self.k.backing_len(*backing).is_ok())) else {
                    return Ok(Outcome::Skipped);
                };
                done(self.k.retain_backing(process, backing))
            }
            40 => {
                let (Some(process), Some(backing)) = (r.prefer(&self.processes, self.alive()), r.prefer(&self.backings, |backing| self.k.backing_len(*backing).is_ok())) else {
                    return Ok(Outcome::Skipped);
                };
                done(self.quiet(|k| k.release_backing(process, backing)))
            }
            41..=44 => {
                let Some(process) = r.prefer(&self.processes, self.alive()) else { return Ok(Outcome::Skipped) };
                let (first, count) = (r.below(PAGES), r.below(4).saturating_add(1));
                let source = if r.below(3) == 0 { None } else { r.prefer(&self.backings, |backing| self.k.backing_len(*backing).is_ok()).map(|b| (b, r.below(3).saturating_mul(PAGE))) };
                let prot = match r.below(4) {
                    0 => Prot::NONE,
                    1 => Prot::READ,
                    2 => Prot::READ_WRITE,
                    _ => Prot::READ_EXECUTE,
                };
                let map_source = match source {
                    None => MapSource::Reserved,
                    Some((backing, offset)) => MapSource::Backing { backing, offset: BackingOffset::new(offset) },
                };
                let range = pages(first, count).ok_or("範囲")?;
                let overlap = self.spaces.get(&process).is_some_and(|space| space.any(first, count));
                let result = self.k.map(process, range, map_source, prot);
                match result {
                    Ok(_) if overlap => return Err(format!("MEM-2: 重なる範囲へのマッピングを受理した: {range:?}")),
                    Err(Error::Refused(Refusal::Overlap)) if !overlap => {
                        return Err(format!("モデルでは空いている範囲を重なりとして拒否した: {range:?}"));
                    }
                    Ok(_) => {
                        let space = self.spaces.get_mut(&process).ok_or("モデルに無いプロセスへのマッピングを受理した")?;
                        space.map(first, count, source, if source.is_some() { prot } else { Prot::NONE }, PAGE);
                        self.check_space(process)?;
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            45..=47 => {
                let Some(process) = r.prefer(&self.processes, self.alive()) else { return Ok(Outcome::Skipped) };
                let (first, count) = (r.below(PAGES), r.below(6).saturating_add(1));
                let range = pages(first, count).ok_or("範囲")?;
                match self.k.unmap(process, range) {
                    Ok(bytes) => {
                        let space = self.spaces.get_mut(&process).ok_or("モデルに無いプロセスの unmap を受理した")?;
                        let (removed, backed) = space.unmap(first, count);
                        if bytes != removed.saturating_mul(PAGE) {
                            return Err(format!("外したバイト数 {bytes}、モデルは {removed} ページ"));
                        }
                        if backed {
                            for held in self.held.iter_mut().filter(|held| held.permit.process() == process) {
                                held.stale = true;
                            }
                        }
                        self.check_space(process)?;
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            48..=50 => {
                let Some(process) = r.prefer(&self.processes, self.alive()) else { return Ok(Outcome::Skipped) };
                let (first, count) = (r.below(PAGES), r.below(4).saturating_add(1));
                let prot = if r.flag() { Prot::READ } else { Prot::READ_WRITE };
                let range = pages(first, count).ok_or("範囲")?;
                let coverable = self.spaces.get(&process).is_some_and(|space| space.all_backed(first, count));
                match self.k.protect(process, range, prot) {
                    Ok(_) if !coverable => {
                        return Err(format!("実体の無い部分を含む範囲の権限変更を受理した: {range:?}"));
                    }
                    Err(Error::Refused(Refusal::NotMapped | Refusal::NotBacked)) if coverable => {
                        return Err(format!("モデルではすべて実体のある範囲を拒否した: {range:?}"));
                    }
                    Ok(_) => {
                        self.spaces.get_mut(&process).ok_or("モデル")?.protect(first, count, prot);
                        for held in self.held.iter_mut().filter(|held| held.permit.process() == process) {
                            held.stale = true;
                        }
                        self.check_space(process)?;
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            51 => {
                // 写しを取り、別の実行インスタンスとして復元して、以後はそちらを動かす。
                if r.below(4) != 0 {
                    return Ok(Outcome::Skipped);
                }
                let snapshot = self.k.snapshot().map_err(|error| format!("写しを取れない: {error:?}"))?;
                if self.k != snapshot {
                    return Err("写しが元の状態と違う".to_owned());
                }
                if snapshot.restore(self.k.instance()).err() != Some(Error::Refused(Refusal::SameInstance)) {
                    return Err("同じ実行インスタンスとして復元できた".to_owned());
                }
                let quiet = self.held.is_empty() && self.in_flight.is_empty();
                let next = InstanceId::new(self.restores.saturating_add(100));
                match snapshot.restore(next) {
                    Ok(restored) if quiet => {
                        if restored.instance() != next || restored.census() != self.k.census() {
                            return Err("復元した状態が写しと違う".to_owned());
                        }
                        self.k = restored;
                        self.restores = self.restores.saturating_add(1);
                        Outcome::Accepted
                    }
                    Ok(_) => return Err("実行権か I/O が外に残っている写しを復元できた".to_owned()),
                    Err(Error::Refused(Refusal::PermitsLent)) if !self.held.is_empty() => Outcome::Skipped,
                    Err(Error::Refused(Refusal::IoInFlight)) if self.held.is_empty() && !self.in_flight.is_empty() => {
                        Outcome::Skipped
                    }
                    Err(error) => return Err(format!("復元の拒否が状況と合わない: {error:?}")),
                }
            }
            _ => Outcome::Skipped,
        })
    }

    fn step(&mut self, r: &mut Reader<'_>) -> Result<(), String> {
        let code = r.byte();
        let before = self.k.snapshot().map_err(|error| format!("写しを取れない: {error:?}"))?;
        let seen = self.k.pending_effects();
        match self.op(code & 0x3f, r)? {
            Outcome::Accepted => self.accepted = self.accepted.saturating_add(1),
            Outcome::Skipped => {}
            Outcome::Refused(Error::Broken(invariant)) => {
                return Err(format!("{}: 内部の不変条件違反を報告した", invariant.code()));
            }
            Outcome::Refused(error) => {
                self.refused = self.refused.saturating_add(1);
                if self.k != before {
                    return Err(format!("失敗した操作 ({error:?}) が状態を変えた"));
                }
            }
        }
        self.k.audit().map_err(|violation| format!("{}: {}", violation.invariant().code(), violation.detail()))?;
        self.observe(seen)?;
        // 実行権を返したスレッドの古い世代の印は、Held ごと消えている。
        self.check_counts()?;
        if code & 0x40 != 0 {
            while self.quiet(|k| k.poll_effect()).is_some() {}
        }
        Ok(())
    }
}

/// バイト列を操作列として実行し、すべての性質を確かめる。
pub fn drive(bytes: &[u8]) -> Result<Summary, Failure> {
    drive_probed(bytes, Probe::NONE)
}

/// [`drive`] と同じ。確保しないと約束している操作の前後で `probe` を呼ぶ。
pub fn drive_probed(bytes: &[u8], probe: Probe) -> Result<Summary, Failure> {
    let mut sim = Sim::new(probe).map_err(|message| Failure { step: 0, message })?;
    let mut reader = Reader { bytes };
    let mut steps = 0usize;
    while !reader.done() {
        sim.step(&mut reader).map_err(|message| Failure { step: steps, message })?;
        steps = steps.saturating_add(1);
    }
    Ok(Summary { steps, accepted: sim.accepted, refused: sim.refused, effects: sim.effects, kernel: sim.k, restores: sim.restores })
}
