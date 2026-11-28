//! バイト列で駆動する決定的シミュレーションと、履歴の監視。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::num::NonZeroU32;

use kome::{
    Access, AccessError, AddrWidth, BackingId, BackingOffset, Budget, CancelReply, ChannelId, ChannelSpec, Completion, CompletionKind,
    Deadline, Duration, Effect, End, Error, ExitCode, GuestAddr, GuestRange, HostError, InstanceId, Instant, Interruptible, IoId, IoLinks,
    IoOutcome, IoPhase, Kernel, Limits, MapSource, MemFault, MemFaultKind, NotRelinquished, ObjectId, PageSize, Parcel, Period, Priority, ProcessId, ProcessPhase, Prot, Quota,
    Received, Refusal, Resource, RunPermit, Sent, SignalMode, SignalOp, SignalSpec, Start, Stop, Tag, ThreadId, ThreadPhase, ThreadFate, TimerId, WaitMode, WaitOutcome,
    WaitStart, WorkerId, guest_read, guest_write_watched, map_observed, protect_observed, unmap_observed,
};
use kome_host_std::InterruptSlot;

use crate::pages::{Expect, Pages, Perm, Touch};
use crate::pipes::{Got, Pipe, Put, Shape};
use crate::shadow::{Dropped, Flaky, Shadow, Store};

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
        .with_channels(3)
        .with_channel_bytes(16)
        .with_quantum(Budget::new(10))
        // advance が途中で区切って返る経路を通すため、目安を小さくする。
        .with_advance_batch(NonZeroU32::MIN.saturating_add(1))
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
    /// 通信路の操作が到達した結果の種類。操作列が空回りしていないことを試験が確かめる。
    pub reached: BTreeSet<&'static str>,
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
    /// この実行へ中断が要求されたか。
    interrupted: bool,
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
    channels: Vec<ChannelId>,
    /// 生きている通信路の独立したモデルと、その通知の資源 (読める、書ける)。
    pipes: BTreeMap<ChannelId, (Pipe<ProcessId, Parcel>, ObjectId, ObjectId)>,
    held: Vec<Held>,
    /// worker ごとの、中断の求めの置き場。参照ホストが使うものと同じ表現で、`Effect::Interrupt` を
    /// 受け取るたびに求めを立てる。前の実行への求めは、消さずに残す (参照ホストも消さない)。
    slots: Vec<InterruptSlot>,
    /// worker ごとの、最後に中断を求めた実行のスレッド。
    asked: BTreeMap<WorkerId, ThreadId>,
    /// 自分で設定した優先度。設定していないスレッドは既定の値。
    ranks: BTreeMap<ThreadId, u8>,
    /// 貸せる状態 (実行待ちで、停止要求が無い) のスレッドと、その状態になった操作の番号。
    since: BTreeMap<ThreadId, usize>,
    steps: usize,
    /// 自分で設定した、プロセスへの割り当て (スレッドの数、I/O 要求の数、実体つきのバイト数)。
    quotas: BTreeMap<ProcessId, (u32, u32, u64)>,
    /// I/O 要求を作ったプロセス。
    io_owner: BTreeMap<IoId, ProcessId>,
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
    /// 通知だけから組み立てたアドレス空間。
    shadow: Shadow,
    /// ゲストのアドレスでの読み書きが使う、実体の内容。
    store: Store,
    /// モデルのページから自分で引いて書いた、あるはずの内容。
    content: Store,
    effects: Vec<Effect>,
    accepted: usize,
    refused: usize,
    /// 復元した回数。復元のたびに新しい実行インスタンスの識別を作る。
    restores: u64,
    reached: BTreeSet<&'static str>,
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
            channels: Vec::new(),
            pipes: BTreeMap::new(),
            held: Vec::new(),
            slots: (0..WORKERS).map(|_| InterruptSlot::new()).collect(),
            asked: BTreeMap::new(),
            ranks: BTreeMap::new(),
            since: BTreeMap::new(),
            steps: 0,
            quotas: BTreeMap::new(),
            io_owner: BTreeMap::new(),
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
            shadow: Shadow::new(PAGE),
            store: Store::default(),
            content: Store::default(),
            effects: Vec::new(),
            accepted: 0,
            refused: 0,
            restores: 0,
            reached: BTreeSet::new(),
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
                    .iter_mut()
                    .find(|held| held.permit.worker() == worker && held.permit.thread() == thread && held.permit.run() == run);
                let Some(held) = known else { return Err(format!("貸していない実行への中断: {effect:?}")) };
                if held.interrupted {
                    return Err(format!("RUN-1: 一度の実行へ中断を二度要求した: {effect:?}"));
                }
                held.interrupted = true;
                let slot = usize::try_from(worker.get()).ok().and_then(|at| self.slots.get(at));
                slot.ok_or(format!("置き場の無い worker への中断: {effect:?}"))?.ask(run);
                self.asked.insert(worker, thread);
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
                // 通信路の端は、observe が先に外している。
                self.spaces.remove(&process);
                kome::SpaceObserver::cleared(&mut self.shadow, process);
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
                // 両端の保持が無くなった通信路は回収され、その通知の資源はここで破棄される。
                for (channel, (pipe, readable, writable)) in self.pipes.iter().filter(|(_, (pipe, ..))| !pipe.gone()) {
                    if *readable == object || *writable == object || pipe.parcels().any(|parcel| parcel == Parcel::Object(object)) {
                        return Err(format!("IPC-1: 通信路 {channel:?} が使っている資源を破棄した: {object:?}"));
                    }
                }
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
                if self.pipes.values().any(|(pipe, ..)| pipe.parcels().any(|parcel| parcel == Parcel::Backing(backing))) {
                    return Err(format!("IPC-1: 通信路が運んでいる実体を解放した: {backing:?}"));
                }
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
        // 終了したプロセスの端は、終了の通知より先に外れている。捨てた中身が運んでいた資源の破棄も、
        // 通知より先に届く。モデルの端を先に外してから、順に見る。
        let mut at = seen;
        while let Some(effect) = self.k.peek_effect(at) {
            if let Effect::ProcessExited { process, .. } = effect {
                for (pipe, ..) in self.pipes.values_mut() {
                    pipe.forget(process);
                }
            }
            at = at.saturating_add(1);
        }
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

    /// 自分で設定した割り当て。設定していなければ、割り当てなし。
    fn share(&self, process: ProcessId) -> (u32, u32, u64) {
        self.quotas.get(&process).copied().unwrap_or((u32::MAX, u32::MAX, u64::MAX))
    }

    /// 作る操作の結果が、割り当て `quota` と今の使用量 `used` (作った後の量は `after`) に合っているか。
    fn within(&mut self, result: Result<(), Error>, over: Resource, after: u64, quota: u64, tag: &'static str) -> Result<(), String> {
        match result {
            Ok(()) if after > quota => Err(format!("割り当て {quota} を超える {after} を受理した ({over:?})")),
            Err(Error::Exhausted(resource)) if resource == over && after <= quota => {
                Err(format!("割り当て {quota} に収まる {after} を {over:?} で拒否した"))
            }
            Err(Error::Exhausted(resource)) if resource == over => {
                self.reached.insert(tag);
                Ok(())
            }
            Ok(()) | Err(_) => Ok(()),
        }
    }

    fn rank(&self, thread: ThreadId) -> u8 {
        self.ranks.get(&thread).copied().unwrap_or(Priority::DEFAULT.get())
    }

    /// 古い世代の実行が残っていないプロセスのスレッドか (残っていれば貸せない)。
    fn open(&self, thread: ThreadId) -> bool {
        self.k.process_of(thread).is_ok_and(|process| !self.held.iter().any(|held| held.permit.process() == process && held.stale))
    }

    /// 貸せる状態のスレッドの記録を、今の状態に合わせる。新しく貸せる状態になったものには、今の操作の番号を付ける。
    fn note_ready(&mut self) {
        for thread in &self.threads {
            let ready = self.k.thread_phase(*thread) == Ok(ThreadPhase::Ready) && self.k.suspend_count(*thread) == Ok(0);
            if ready {
                self.since.entry(*thread).or_insert(self.steps);
            } else {
                self.since.remove(thread);
            }
        }
    }

    /// 貸された実行権を確かめて記録する。`named` は、スレッドを名指しして借りたか。
    fn lent(&mut self, permit: RunPermit, named: bool) -> Result<(), String> {
        let (process, picked) = (permit.process(), permit.thread());
        for held in &self.held {
            if held.permit.thread() == picked || held.permit.worker() == permit.worker() {
                return Err(format!("RUN-1: 実行権が重複した: {permit:?}"));
            }
            if held.permit.process() == process && held.stale {
                return Err(format!("MEM-3: 古い世代の実行が残るプロセスへ貸した: {permit:?}"));
            }
        }
        if permit.instance() != self.k.instance() || Ok(permit.map()) != self.k.map_generation(process) {
            return Err(format!("実行権の内容が状態と一致しない: {permit:?}"));
        }
        let Some(mine) = self.since.get(&picked).copied() else {
            return Err(format!("RUN-2: 貸せる状態でないスレッドへ貸した: {permit:?}"));
        };
        if !named {
            // 貸す順序: 優先度が最も高いものの中で、最も先に貸せる状態になったもの。
            for (other, theirs) in &self.since {
                if *other == picked || !self.open(*other) {
                    continue;
                }
                if self.rank(*other) > self.rank(picked) {
                    return Err(format!("RUN-2: 優先度の高い {other:?} を飛ばして {picked:?} へ貸した"));
                }
                if self.rank(*other) == self.rank(picked) && *theirs < mine {
                    return Err(format!("RUN-2: 先に実行待ちになった {other:?} を飛ばして {picked:?} へ貸した"));
                }
                if self.rank(*other) < self.rank(picked) && *theirs < mine {
                    self.reached.insert("run:outranked");
                }
            }
        }
        // 前の実行への中断の求めが置き場に残ったまま、同じ worker が次の実行を借りた。
        match self.asked.get(&permit.worker()) {
            Some(earlier) if *earlier == picked => self.reached.insert("run:again-after-interrupt"),
            Some(_) => self.reached.insert("run:other-thread-after-interrupt"),
            None => false,
        };
        let backings = self.spaces.get(&process).map(|space| space.backings().collect()).unwrap_or_default();
        self.held.push(Held { permit, backings, stale: false, interrupted: false });
        Ok(())
    }

    /// 参照ホストの表現で見た中断の求めが、Kernel がその実行へ中断を要求したかと一致するか。
    /// 別のスレッドや前の実行への求めを、今の実行への求めと取り違えない。
    fn check_interrupts(&self) -> Result<(), String> {
        for held in &self.held {
            let slot = usize::try_from(held.permit.worker().get()).ok().and_then(|at| self.slots.get(at));
            let seen = slot.ok_or("置き場の無い worker へ貸した")?.watch().requested(&held.permit);
            if seen != held.interrupted {
                return Err(format!("RUN-1: {:?} への中断の求めが、ホストから見ると {seen}、Kernel の要求は {}", held.permit, held.interrupted));
            }
        }
        Ok(())
    }

    /// 実体の `places` のバイトを、実行できる権限で見せている (プロセス, アドレス) の全て。モデルのページから引く。
    fn shown_as_code(&self, places: &[(BackingId, u64)]) -> BTreeSet<(ProcessId, u64)> {
        let mut shown = BTreeSet::new();
        for (process, model) in &self.spaces {
            for (backing, offset) in places {
                shown.extend(model.executes(*backing, *offset, PAGE).map(|addr| (*process, addr)));
            }
        }
        shown
    }

    /// モデルとカーネルで、すべての通信路の中身の量、次の長さ、読める・書けるの通知を比べる。
    /// 両端の保持が無くなった通信路は、回収されていることを確かめてモデルから外す。
    fn check_pipes(&mut self) -> Result<(), String> {
        let mut reclaimed = Vec::new();
        for (channel, (pipe, readable, writable)) in &self.pipes {
            let queued = self.k.channel_queued(*channel);
            if pipe.gone() {
                if queued != Err(Error::Stale(Tag::Channel)) {
                    return Err(format!("IPC-1: 両端の保持が無い通信路が残っている: {channel:?} {queued:?}"));
                }
                reclaimed.push(*channel);
                self.reached.insert("reclaimed");
                continue;
            }
            let wide = |len: usize| u64::try_from(len).map_err(|error| error.to_string());
            let next = pipe.next_len().map(wide).transpose()?;
            if queued != Ok(wide(pipe.queued())?) || self.k.next_message_len(*channel) != Ok(next) {
                return Err(format!("IPC-1: 通信路 {channel:?} の中身が {queued:?}、モデルは {}", pipe.queued()));
            }
            let levels = (self.k.signal_count(*readable), self.k.signal_count(*writable));
            if levels != (Ok(u64::from(pipe.readable())), Ok(u64::from(pipe.writable()))) {
                return Err(format!(
                    "IPC-1: 通信路 {channel:?} の通知が {levels:?}、モデルは 読める {} 書ける {}",
                    pipe.readable(),
                    pipe.writable()
                ));
            }
        }
        for channel in reclaimed {
            self.pipes.remove(&channel);
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
            for (access, touch) in [(Access::Read, Touch::Read), (Access::Write, Touch::Write), (Access::Execute, Touch::Execute)] {
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
                let expected = model.expect(page, within, touch);
                if actual != expected {
                    return Err(format!("ページ {page} の {access:?}: カーネル {actual:?}、モデル {expected:?}"));
                }
            }
        }
        Ok(())
    }

    /// 通知だけから組み立てたアドレス空間が、モデルと同じか。世代も、最後に知らされたものが今の世代か。
    fn check_shadow(&self, process: ProcessId) -> Result<(), String> {
        let Some(model) = self.spaces.get(&process) else { return Ok(()) };
        let seen = self.shadow.space(process).cloned().unwrap_or_default();
        if seen != *model {
            return Err(format!("MEM-2: 通知から組み立てた {process:?} のアドレス空間が、モデルと違う"));
        }
        let told = self.shadow.generation(process);
        if told.is_some() && told != self.k.map_generation(process).ok() {
            return Err(format!("MEM-3: 最後に知らされた世代 {told:?} が、{process:?} の今の世代と違う"));
        }
        Ok(())
    }

    /// モデルのページから引いた、`len` バイトの行き先。届かない位置があれば、最初のものの種類。
    fn reach(&self, process: ProcessId, start: u64, len: u64, touch: Touch) -> Result<Vec<(BackingId, u64)>, (u64, MemFaultKind)> {
        let model = self.spaces.get(&process);
        let mut places = Vec::new();
        for addr in start..start.saturating_add(len) {
            let expected = model.map_or(Expect::Unmapped, |model| model.expect(addr / PAGE, addr % PAGE, touch));
            match expected {
                Expect::Reaches(backing, offset) => places.push((backing, offset)),
                Expect::Unmapped => return Err((addr, MemFaultKind::Unmapped)),
                Expect::Reserved => return Err((addr, MemFaultKind::Reserved)),
                Expect::Denied => return Err((addr, MemFaultKind::Denied)),
            }
        }
        Ok(places)
    }

    /// 権限を変えた範囲とその両隣に、一つにまとめられる項目が分かれたまま残っていないか。
    /// 変更を繰り返しても、項目の数は見え方の違いの数を超えない。
    fn check_merged(&self, process: ProcessId, first: u64, count: u64) -> Result<(), String> {
        let model = self.spaces.get(&process).ok_or("モデル")?;
        let mut from = GuestAddr::new(0);
        let mut previous: Option<GuestRange> = None;
        while let Some(mapping) = self.k.next_mapping(process, from).map_err(|error| format!("{error:?}"))? {
            let range = mapping.range();
            let page = range.start().get() / PAGE;
            let touches = previous.is_some_and(|before| before.last().get().checked_add(1) == Some(range.start().get()));
            let near = page >= first && page <= first.saturating_add(count);
            if touches && near && model.continues(page, PAGE) {
                return Err(format!("MEM-2: まとめられる項目が分かれたまま残った: ページ {page} の前後"));
            }
            previous = Some(range);
            let Some(next) = range.last().get().checked_add(1) else { break };
            from = GuestAddr::new(next);
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
                let live = self.threads.iter().filter(|thread| self.k.process_of(**thread) == Ok(process)).count();
                let after = u64::try_from(live).map_err(|e| e.to_string())?.saturating_add(1);
                let made = self.k.create_thread(process, start);
                self.within(made.map(|_| ()), Resource::ProcessThreads, after, u64::from(self.share(process).0), "quota:threads")?;
                match made {
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
                        self.lent(permit, false)?;
                        Outcome::Accepted
                    }
                    Ok(None) => {
                        if let Some(idle) = self.since.keys().find(|thread| self.open(**thread)) {
                            return Err(format!("RUN-2: 貸せるスレッド {idle:?} があるのに貸さなかった"));
                        }
                        Outcome::Accepted
                    }
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
                let fault = MemFault::new(GuestAddr::new(u64::from(r.byte())), MemFaultKind::Unmapped);
                let (stop, tag) = match r.below(8) {
                    0 | 1 => (Stop::Trap, "run:trap"),
                    2 => (Stop::Yielded, "run:yielded"),
                    3 | 4 => (Stop::BudgetExhausted, "run:budget"),
                    5 => (Stop::Interrupted, "run:interrupted"),
                    6 => (Stop::Fault(fault), "run:fault"),
                    _ => (Stop::Exited(ExitCode::new(u64::from(r.byte()))), "run:exited"),
                };
                let consumed = Budget::new(u64::from(r.byte() & 0x1f));
                // 状況と合わない理由は、実行権を消費せずに拒否される。
                let mismatch = match stop {
                    Stop::BudgetExhausted if consumed < limits().quantum() => Some((Refusal::BudgetRemains, "run:budget-remains")),
                    Stop::Interrupted if !held.interrupted => Some((Refusal::NotInterrupted, "run:not-interrupted")),
                    Stop::BudgetExhausted | Stop::Yielded | Stop::Interrupted | Stop::Trap | Stop::Fault(_) | Stop::Exited(_) => None,
                };
                // 届け先を間違えても実行権は失われない: 別の実行インスタンスは、実行を始める前に
                // 受け取らずに返す。返すときも同じ。
                let mut stranger = Kernel::new(InstanceId::new(u64::MAX), Limits::DEFAULT.with_effects(1))
                    .map_err(|error| format!("別の実行インスタンスを作れない: {error:?}"))?;
                let permit = match kome::start_run(&mut stranger, held.permit, &mut kome::Contexts::<()>::new()) {
                    Err(kome::NotRun::Rejected { permit, loaded: None, error: Error::Foreign }) => permit,
                    other => return Err(format!("RUN-1: 別の実行インスタンスが実行を始めた、または実行権を返さなかった: {other:?}")),
                };
                let permit = match stranger.relinquish(permit, stop, consumed) {
                    Err(NotRelinquished::Rejected(permit, Error::Foreign)) => permit,
                    other => return Err(format!("RUN-1: 別の実行インスタンスが実行権を受理した、または返さなかった: {other:?}")),
                };
                let (permit, stop) = match (self.quiet(|k| k.relinquish(permit, stop, consumed)), mismatch) {
                    (Err(NotRelinquished::Rejected(permit, Error::Refused(refusal))), Some((expected, tag))) if refusal == expected => {
                        self.reached.insert(tag);
                        (Some(permit), Stop::Trap)
                    }
                    (Ok(fate), None) => {
                        let fits = match (fate, stop) {
                            // 実行中に受けた終了要求は、どの理由よりも先に効く。
                            (ThreadFate::Exited(_), _) => true,
                            (ThreadFate::Ready, Stop::BudgetExhausted | Stop::Yielded | Stop::Interrupted) => true,
                            (ThreadFate::Servicing, Stop::Trap | Stop::Fault(_)) => true,
                            (ThreadFate::Ready | ThreadFate::Servicing, _) => false,
                        };
                        if !fits {
                            return Err(format!("RUN-1: 理由 {stop:?} で返したスレッドが {fate:?} になった"));
                        }
                        self.reached.insert(tag);
                        (None, stop)
                    }
                    (result, mismatch) => return Err(format!("RUN-1: 理由 {stop:?} の返却が {result:?}、見込みは {mismatch:?}")),
                };
                match permit.map(|permit| self.quiet(|k| k.relinquish(permit, stop, consumed))) {
                    Some(Ok(ThreadFate::Servicing | ThreadFate::Exited(_))) | None => Outcome::Accepted,
                    Some(other) => return Err(format!("RUN-1: 拒否された後の正当な返却が {other:?}")),
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
                        if self.blocked.contains_key(&thread) {
                            return Err(format!("WAIT-1: 待機中のスレッドがもう一つ待機を登録した: {thread:?}"));
                        }
                        self.blocked.insert(thread, targets);
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
                // 一度の advance は目安の数で区切って返る。残っている間は同じ時刻で呼び直し、
                // 区切りごとに、時刻が戻らず、渡した時刻を越えず、仕事が進んでいることを確かめる。
                loop {
                    let before = self.k.now();
                    let advanced = match self.quiet(|k| k.advance(to)) {
                        Ok(advanced) => advanced,
                        Err(error) => break done(Err(error)),
                    };
                    if advanced.reached() != self.k.now() || advanced.reached() < before || advanced.reached() > to {
                        return Err(format!("advance の到達時刻がおかしい: {advanced:?} (前 {before:?}、先 {to:?})"));
                    }
                    if !advanced.more() {
                        if advanced.reached() != to {
                            return Err(format!("残りが無いのに、渡した時刻に届いていない: {advanced:?}"));
                        }
                        break done(Ok(()));
                    }
                    if advanced.fired() == 0 {
                        return Err(format!("残りがあるのに、一つも処理していない: {advanced:?}"));
                    }
                }
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
                let live = self.io_owner.iter().filter(|(io, owner)| **owner == process && self.k.io_phase(**io).is_ok()).count();
                let after = u64::try_from(live).map_err(|e| e.to_string())?.saturating_add(1);
                let made = self.k.prepare_io(process, links);
                self.within(made.map(|_| ()), Resource::ProcessIos, after, u64::from(self.share(process).1), "quota:ios")?;
                match made {
                    Ok(io) => {
                        self.ios.push(io);
                        self.io_owner.insert(io, process);
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
            44 => {
                // ゲストのアドレスでの読み書き。ページの境界をまたぎやすい位置を選ぶ。
                let Some(process) = r.prefer(&self.processes, self.alive()) else { return Ok(Outcome::Skipped) };
                let start = r.below(PAGES).saturating_add(1).saturating_mul(PAGE).saturating_sub(r.below(4));
                let len = r.below(7);
                let size = usize::try_from(len).map_err(|e| e.to_string())?;
                let writing = r.flag();
                let data: Vec<u8> = (0..size).map(|_| r.byte() | 1).collect();
                let mut read = vec![0u8; size];
                let expected = self.reach(process, start, len, if writing { Touch::Write } else { Touch::Read });
                let before = self.store.clone();
                // 書き込みは監視つきの経路を通す。8 回に 1 回は、backend が途中のバイトで失敗する。
                let breaks = if writing { Some(r.byte()).filter(|byte| byte & 7 == 7).map(|byte| u64::from(byte >> 3) % len.max(1)) } else { None };
                let breaks = breaks.filter(|_| len != 0);
                let mut dropped = Dropped::default();
                let result = if writing {
                    let mut flaky = Flaky { store: &mut self.store, left: breaks };
                    guest_write_watched(&self.k, &mut flaky, &mut dropped, process, GuestAddr::new(start), &data)
                } else {
                    guest_read(&self.k, &self.store, process, GuestAddr::new(start), &mut read)
                };
                // 捨てさせるコードは、書いた実体を実行できる権限で見せている場所の全てで、それ以外は無い。
                // 書き込み先とは別の範囲や別のプロセス (別名) も入る。途中で失敗しても、範囲の全体について捨てさせる。
                let code = match &expected {
                    Ok(places) if writing => self.shown_as_code(places),
                    Ok(_) | Err(_) => BTreeSet::new(),
                };
                if dropped.bytes != code || dropped.forgotten != 0 {
                    return Err(format!("{start:#x} へ {len} バイト書いて捨てさせたコードが {:?}、モデルでは {code:?}", dropped.bytes));
                }
                if writing && expected.is_ok() {
                    let alias = code.iter().any(|(shown_in, addr)| *shown_in != process || *addr < start || *addr >= start.saturating_add(len));
                    self.reached.insert(match (code.is_empty(), alias, breaks.is_some()) {
                        (true, _, _) => "code:kept",
                        (false, _, true) => "code:dropped-after-failure",
                        (false, true, false) => "code:dropped-alias",
                        (false, false, false) => "code:dropped",
                    });
                }
                match (result, expected) {
                    (Err(AccessError::Host(error)), Ok(places)) if breaks.is_some() && error == Flaky::BROKE => {
                        // 失敗する前に書いたバイトだけが変わっている。
                        let wrote = usize::try_from(breaks.unwrap_or(0)).map_err(|e| e.to_string())?;
                        for (at, (backing, offset)) in places.iter().enumerate().take(wrote) {
                            self.content.set(*backing, *offset, data.get(at).copied().ok_or("書くバイト")?);
                        }
                        if self.store != self.content {
                            return Err(format!("MEM-2: {start:#x} への途中で失敗した書き込みの後、実体の内容がモデルと違う"));
                        }
                        self.reached.insert("mem:write-broke");
                        Outcome::Accepted
                    }
                    (Ok(()), Ok(places)) if breaks.is_none() => {
                        for (at, (backing, offset)) in places.iter().enumerate() {
                            if writing {
                                self.content.set(*backing, *offset, data.get(at).copied().ok_or("書くバイト")?);
                            } else if read.get(at).copied() != Some(self.content.byte(*backing, *offset)) {
                                return Err(format!("MEM-2: {start:#x} から読んだ {read:?} の {at} 番目が、モデルの内容と違う"));
                            }
                        }
                        if self.store != self.content {
                            return Err(format!("MEM-2: {start:#x} への書き込みの後、実体の内容がモデルと違う"));
                        }
                        let across = len != 0 && start / PAGE != start.saturating_add(len).saturating_sub(1) / PAGE;
                        self.reached.insert(match (writing, across) {
                            (true, _) => "mem:written",
                            (false, false) => "mem:read",
                            (false, true) => "mem:read-across",
                        });
                        Outcome::Accepted
                    }
                    (Err(AccessError::Fault(fault)), Err((addr, kind))) if fault.addr() == GuestAddr::new(addr) && fault.kind() == kind => {
                        if self.store != before {
                            return Err(format!("MEM-2: 失敗した {start:#x} へのアクセスが内容を変えた"));
                        }
                        self.reached.insert(if addr == start { "mem:fault" } else { "mem:fault-inside" });
                        Outcome::Accepted
                    }
                    (Err(AccessError::Kernel(error)), _) => Outcome::Refused(error),
                    (result, expected) => return Err(format!("MEM-2: {start:#x} から {len} バイトのアクセスが {result:?}、モデルは {expected:?}")),
                }
            }
            41..=43 => {
                let Some(process) = r.prefer(&self.processes, self.alive()) else { return Ok(Outcome::Skipped) };
                let (first, count) = (r.below(PAGES), r.below(4).saturating_add(1));
                let source = if r.below(3) == 0 { None } else { r.prefer(&self.backings, |backing| self.k.backing_len(*backing).is_ok()).map(|b| (b, r.below(3).saturating_mul(PAGE))) };
                let perm = Perm::from_bits(r.byte() & 7);
                let prot = Prot::new(perm.read, perm.write, perm.execute);
                let map_source = match source {
                    None => MapSource::Reserved,
                    Some((backing, offset)) => MapSource::Backing { backing, offset: BackingOffset::new(offset) },
                };
                let range = pages(first, count).ok_or("範囲")?;
                let overlap = self.spaces.get(&process).is_some_and(|space| space.any(first, count));
                let result = map_observed(&mut self.k, &mut self.shadow, process, range, map_source, prot);
                if source.is_some() {
                    let used = self.spaces.get(&process).map_or(0, Pages::backed_pages);
                    let after = used.saturating_add(count).saturating_mul(PAGE);
                    self.within(result.map(|_| ()), Resource::MappedBytes, after, self.share(process).2, "quota:mapped")?;
                }
                match result {
                    Ok(_) if overlap => return Err(format!("MEM-2: 重なる範囲へのマッピングを受理した: {range:?}")),
                    Err(Error::Refused(Refusal::Overlap)) if !overlap => {
                        return Err(format!("モデルでは空いている範囲を重なりとして拒否した: {range:?}"));
                    }
                    Ok(_) => {
                        let space = self.spaces.get_mut(&process).ok_or("モデルに無いプロセスへのマッピングを受理した")?;
                        space.map(first, count, source, if source.is_some() { perm } else { Perm::NONE }, PAGE);
                        self.check_space(process)?;
                        self.check_shadow(process)?;
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            45..=47 => {
                let Some(process) = r.prefer(&self.processes, self.alive()) else { return Ok(Outcome::Skipped) };
                let (first, count) = (r.below(PAGES), r.below(6).saturating_add(1));
                let range = pages(first, count).ok_or("範囲")?;
                match unmap_observed(&mut self.k, &mut self.shadow, process, range) {
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
                        self.check_shadow(process)?;
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            48..=50 => {
                let Some(process) = r.prefer(&self.processes, self.alive()) else { return Ok(Outcome::Skipped) };
                let (first, count) = (r.below(PAGES), r.below(4).saturating_add(1));
                let perm = Perm::from_bits(r.byte() & 7);
                let prot = Prot::new(perm.read, perm.write, perm.execute);
                let range = pages(first, count).ok_or("範囲")?;
                let coverable = self.spaces.get(&process).is_some_and(|space| space.all_backed(first, count));
                let before = self.k.map_generation(process);
                let pending = self.k.pending_effects();
                match protect_observed(&mut self.k, &mut self.shadow, process, range, prot) {
                    Ok(_) if !coverable => {
                        return Err(format!("実体の無い部分を含む範囲の権限変更を受理した: {range:?}"));
                    }
                    Err(Error::Refused(Refusal::NotMapped | Refusal::NotBacked)) if coverable => {
                        return Err(format!("モデルではすべて実体のある範囲を拒否した: {range:?}"));
                    }
                    Ok(generation) => {
                        let space = self.spaces.get_mut(&process).ok_or("モデル")?;
                        let (changed, lost) = space.protect(first, count, perm);
                        // 何も変わらない変更は世代を進めない。変わる変更は進める。
                        if (Ok(generation) != before) != changed {
                            return Err(format!("権限の変更 (変化 {changed}) の前後の世代: {before:?} → {generation:?}"));
                        }
                        // 権限が減らない変更は、実行中のスレッドへ中断を要求しない。
                        if !lost && self.k.pending_effects() != pending {
                            return Err(format!("権限が減らない変更で Effect が出た: {range:?}"));
                        }
                        if changed {
                            self.check_merged(process, first, count)?;
                        }
                        if lost {
                            for held in self.held.iter_mut().filter(|held| held.permit.process() == process) {
                                held.stale = true;
                            }
                        }
                        self.check_space(process)?;
                        self.check_shadow(process)?;
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
            52 => {
                let Some(process) = r.prefer(&self.processes, self.alive()) else { return Ok(Outcome::Skipped) };
                let narrow = |value: u64| u32::try_from(value).map_err(|error| error.to_string());
                let size = |value: u64| usize::try_from(value).map_err(|error| error.to_string());
                // 数や容量が 0 の形は拒否される。
                let (spec, shape) = if r.flag() {
                    let bytes = r.below(7);
                    (ChannelSpec::stream(narrow(bytes)?), (bytes != 0).then_some(Shape::Stream(size(bytes)?)))
                } else {
                    let (count, max_len) = (r.below(4), r.below(5));
                    let shape = Shape::Messages(size(count)?, size(max_len)?);
                    (ChannelSpec::messages(narrow(count)?, narrow(max_len)?), (count != 0).then_some(shape))
                };
                match (self.k.create_channel(process, spec), shape) {
                    (Ok(created), Some(shape)) => {
                        fresh(&self.gone_objects, created.readable())?;
                        fresh(&self.gone_objects, created.writable())?;
                        self.channels.push(created.channel());
                        self.objects.push(created.readable());
                        self.objects.push(created.writable());
                        if self.pipes.contains_key(&created.channel()) {
                            return Err(format!("ID-1: 生きている通信路の ID をもう一度発行した: {created:?}"));
                        }
                        let pipe = Pipe::new(shape, process);
                        self.pipes.insert(created.channel(), (pipe, created.readable(), created.writable()));
                        Outcome::Accepted
                    }
                    (Ok(created), None) => return Err(format!("IPC-1: 容量の無い通信路を作れた: {created:?}")),
                    (Err(Error::Refused(Refusal::BadChannelSpec)), Some(shape)) => {
                        return Err(format!("モデルでは作れる形 {shape:?} を拒否した"));
                    }
                    (Err(error), Some(_) | None) => Outcome::Refused(error),
                }
            }
            53 => {
                let Some(channel) = r.prefer(&self.channels, |channel| self.pipes.contains_key(channel)) else { return Ok(Outcome::Skipped) };
                let Some(process) = r.prefer(&self.processes, self.alive()) else { return Ok(Outcome::Skipped) };
                let sending = r.flag();
                let end = if sending { End::Send } else { End::Receive };
                let open = self.pipes.get(&channel).is_some_and(|(pipe, ..)| pipe.open(sending));
                match self.k.retain_channel(process, channel, end) {
                    Ok(()) if !open => return Err(format!("IPC-1: 保持の無い端 {end:?} を増やせた: {channel:?}")),
                    Ok(()) => {
                        let (pipe, ..) = self.pipes.get_mut(&channel).ok_or("モデルに無い通信路の保持を受理した")?;
                        pipe.retain(process, sending);
                        Outcome::Accepted
                    }
                    Err(Error::Refused(Refusal::EndClosed)) if open => {
                        return Err(format!("モデルでは保持のある端 {end:?} を、閉じたとして拒否した: {channel:?}"));
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            54 => {
                let Some(channel) = r.prefer(&self.channels, |channel| self.pipes.contains_key(channel)) else { return Ok(Outcome::Skipped) };
                let sending = r.flag();
                let end = if sending { End::Send } else { End::Receive };
                let holder = |process: &ProcessId| self.pipes.get(&channel).is_some_and(|(pipe, ..)| pipe.holds(*process, sending));
                let Some(process) = r.prefer(&self.processes, holder) else { return Ok(Outcome::Skipped) };
                let held = holder(&process);
                match self.quiet(|k| k.release_channel(process, channel, end)) {
                    Ok(()) if !held => return Err(format!("IPC-1: 保持していない端 {end:?} を手放せた: {process:?} {channel:?}")),
                    Ok(()) => {
                        let (pipe, ..) = self.pipes.get_mut(&channel).ok_or("モデルに無い通信路の解放を受理した")?;
                        if pipe.release(process, sending).iter().any(|parcel| matches!(parcel, Parcel::Object(_) | Parcel::Backing(_))) {
                            self.reached.insert("dropped-parcel");
                        }
                        Outcome::Accepted
                    }
                    Err(error) if held => return Err(format!("保持している端 {end:?} の解放が拒否された: {error:?}")),
                    Err(error) => Outcome::Refused(error),
                }
            }
            55..=57 => {
                let Some(channel) = r.prefer(&self.channels, |channel| self.pipes.contains_key(channel)) else { return Ok(Outcome::Skipped) };
                let holder = |process: &ProcessId| self.pipes.get(&channel).is_some_and(|(pipe, ..)| pipe.holds(*process, true));
                let Some(process) = r.prefer(&self.processes, holder) else { return Ok(Outcome::Skipped) };
                let data: Vec<u8> = (0..r.below(6)).map(|_| r.byte()).collect();
                let parcel = match r.below(6) {
                    0 => r.prefer(&self.objects, self.exists()).map(Parcel::Object),
                    1 => r.prefer(&self.backings, |backing| self.k.backing_len(*backing).is_ok()).map(Parcel::Backing),
                    _ => None,
                };
                // モデルが受け付ける形の送信なら、その結果。長すぎる、流れに資源を載せる、端を持たない送信は拒否。
                let expected = self.pipes.get(&channel).filter(|(pipe, ..)| pipe.holds(process, true)).and_then(|(pipe, ..)| {
                    if parcel.is_some() && !pipe.carries() {
                        return None;
                    }
                    match pipe.clone().put(&data, parcel) {
                        Put::TooLong => None,
                        put @ (Put::Accepted(_) | Put::Full | Put::NoReceiver) => Some(put),
                    }
                });
                match (self.quiet(|k| k.send(process, channel, &data, parcel)), expected) {
                    (Ok(sent), Some(put)) => {
                        let got = match sent {
                            Sent::Accepted(bytes) => Put::Accepted(usize::try_from(bytes).map_err(|error| error.to_string())?),
                            Sent::Full => Put::Full,
                            Sent::NoReceiver => Put::NoReceiver,
                        };
                        if got != put {
                            return Err(format!("IPC-1: 送信の結果が {got:?}、モデルは {put:?}"));
                        }
                        let (pipe, ..) = self.pipes.get_mut(&channel).ok_or("モデルに無い通信路への送信を受理した")?;
                        pipe.put(&data, parcel);
                        self.reached.insert(match (put, parcel) {
                            (Put::Accepted(taken), None) if taken < data.len() => "sent-part",
                            (Put::Accepted(_), None) => "sent",
                            (Put::Accepted(_), Some(Parcel::Object(_))) => "sent-object",
                            (Put::Accepted(_), Some(Parcel::Backing(_))) => "sent-backing",
                            (Put::Full, _) => "full",
                            (Put::NoReceiver, _) => "no-receiver",
                            (Put::TooLong, _) => "too-long",
                        });
                        Outcome::Accepted
                    }
                    (Ok(sent), None) => return Err(format!("IPC-1: モデルでは拒否される送信の結果が {sent:?}")),
                    // 資源を載せた送信は、送り手がその資源を保持していなければ拒否される。モデルは保持を持たない。
                    (Err(error), Some(put)) if parcel.is_none() => {
                        return Err(format!("モデルでは {put:?} になる送信を拒否した: {error:?}"));
                    }
                    (Err(error), Some(_) | None) => {
                        if matches!(error, Error::Refused(Refusal::MessageTooLong)) {
                            self.reached.insert("too-long");
                        }
                        Outcome::Refused(error)
                    }
                }
            }
            58..=60 => {
                let Some(channel) = r.prefer(&self.channels, |channel| self.pipes.contains_key(channel)) else { return Ok(Outcome::Skipped) };
                let holder = |process: &ProcessId| self.pipes.get(&channel).is_some_and(|(pipe, ..)| pipe.holds(*process, false));
                let Some(process) = r.prefer(&self.processes, holder) else { return Ok(Outcome::Skipped) };
                const UNTOUCHED: u8 = 0xee;
                let room = usize::try_from(r.below(7)).map_err(|error| error.to_string())?;
                let mut buffer = vec![UNTOUCHED; room];
                let expected = self.pipes.get(&channel).filter(|(pipe, ..)| pipe.holds(process, false)).map(|(pipe, ..)| pipe.peek(room));
                match (self.k.receive(process, channel, &mut buffer), expected) {
                    (Ok(Received::Data { bytes, parcel }), Some(Got::Data(want, carried))) => {
                        let len = usize::try_from(bytes).map_err(|error| error.to_string())?;
                        let (filled, rest) = buffer.split_at_checked(len).ok_or("IPC-1: 受け取ったバイト数がバッファより長い")?;
                        if filled != want.as_slice() || parcel != carried {
                            return Err(format!("IPC-1: 受け取ったもの {filled:?} {parcel:?}、モデルは {want:?} {carried:?}"));
                        }
                        if rest.iter().any(|byte| *byte != UNTOUCHED) {
                            return Err("IPC-1: 受け取ったバイト数より後ろのバッファを書き換えた".to_owned());
                        }
                        let (pipe, ..) = self.pipes.get_mut(&channel).ok_or("モデルに無い通信路からの受信を受理した")?;
                        pipe.take(len);
                        self.reached.insert(match parcel {
                            None => "received",
                            Some(Parcel::Object(_)) => "received-object",
                            Some(Parcel::Backing(_)) => "received-backing",
                        });
                        Outcome::Accepted
                    }
                    (Ok(Received::Empty), Some(Got::Empty)) => {
                        self.reached.insert("empty");
                        Outcome::Accepted
                    }
                    (Ok(Received::Ended), Some(Got::Ended)) => {
                        self.reached.insert("ended");
                        Outcome::Accepted
                    }
                    (Ok(received), expected) => return Err(format!("IPC-1: 受信の結果が {received:?}、モデルは {expected:?}")),
                    (Err(Error::Refused(Refusal::BufferTooSmall)), Some(Got::TooSmall)) => {
                        self.reached.insert("too-small");
                        Outcome::Refused(Error::Refused(Refusal::BufferTooSmall))
                    }
                    // 資源を運ぶメッセージは、受け手が保持を増やせなければ受け取れない。モデルは保持の上限を持たない。
                    (Err(error), Some(Got::Data(_, Some(_))) | None) => {
                        if buffer.iter().any(|byte| *byte != UNTOUCHED) {
                            return Err("IPC-1: 拒否した受信がバッファを書き換えた".to_owned());
                        }
                        Outcome::Refused(error)
                    }
                    (Err(error), Some(expected)) => return Err(format!("モデルでは {expected:?} になる受信を拒否した: {error:?}")),
                }
            }
            61 if r.below(4) == 0 => {
                let Some(process) = r.prefer(&self.processes, self.alive()) else { return Ok(Outcome::Skipped) };
                let few = |pick: u64| match pick {
                    0 => 1,
                    1 => 2,
                    _ => u32::MAX,
                };
                let (threads, ios) = (few(r.below(3)), few(r.below(3)));
                let mapped = match r.below(3) {
                    0 => 2 * PAGE,
                    1 => 4 * PAGE,
                    _ => u64::MAX,
                };
                let quota = Quota::UNLIMITED.with_threads(threads).with_ios(ios).with_mapped_bytes(mapped);
                match self.quiet(|k| k.set_quota(process, quota)) {
                    Ok(previous) => {
                        if (previous.threads(), previous.ios(), previous.mapped_bytes()) != self.share(process) {
                            return Err(format!("変える前の割り当てが {previous:?}、自分で設定した値は {:?}", self.share(process)));
                        }
                        self.quotas.insert(process, (threads, ios, mapped));
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            61 => {
                let Some(thread) = r.prefer(&self.threads, |thread| self.k.thread_phase(*thread).is_ok()) else { return Ok(Outcome::Skipped) };
                let level = match r.below(3) {
                    0 => 100,
                    1 => Priority::DEFAULT.get(),
                    _ => 200,
                };
                match self.quiet(|k| k.set_priority(thread, Priority::new(level))) {
                    Ok(previous) => {
                        if previous.get() != self.rank(thread) {
                            return Err(format!("変える前の優先度が {previous:?}、自分で設定した値は {}", self.rank(thread)));
                        }
                        self.ranks.insert(thread, level);
                        Outcome::Accepted
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            62 => {
                // 順序をホストが決める貸し方。優先度と待ち順に関係なく、名指ししたスレッドへ貸す。
                let worker = WorkerId::new(u32::try_from(r.below(u64::from(WORKERS))).map_err(|e| e.to_string())?);
                let Some(thread) = r.prefer(&self.threads, |thread| self.since.contains_key(thread)) else { return Ok(Outcome::Skipped) };
                let lendable = self.since.contains_key(&thread) && self.open(thread);
                match self.quiet(|k| k.grant_thread(worker, thread)) {
                    Ok(permit) if permit.thread() != thread => return Err(format!("名指しした {thread:?} と違うスレッドへ貸した: {permit:?}")),
                    Ok(permit) => {
                        self.lent(permit, true)?;
                        self.reached.insert("run:named");
                        Outcome::Accepted
                    }
                    Err(Error::Refused(Refusal::NotReady | Refusal::Barrier)) if lendable => {
                        return Err(format!("RUN-2: 貸せるスレッド {thread:?} の名指しを拒否した"));
                    }
                    Err(error) => Outcome::Refused(error),
                }
            }
            63 => {
                let Some(thread) = r.prefer(&self.threads, self.in_phase(ThreadPhase::Running)) else { return Ok(Outcome::Skipped) };
                let running = self.k.thread_phase(thread) == Ok(ThreadPhase::Running);
                match self.quiet(|k| k.preempt_thread(thread)) {
                    Ok(()) if !running => return Err(format!("実行中でない {thread:?} へ中断を要求できた")),
                    Ok(()) => {
                        self.reached.insert("run:preempted");
                        Outcome::Accepted
                    }
                    Err(error) if running => return Err(format!("実行中の {thread:?} への中断の要求が拒否された: {error:?}")),
                    Err(error) => Outcome::Refused(error),
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
        self.check_interrupts()?;
        self.check_pipes()?;
        self.note_ready();
        self.steps = self.steps.saturating_add(1);
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
    Ok(Summary { steps, accepted: sim.accepted, refused: sim.refused, effects: sim.effects, kernel: sim.k, restores: sim.restores, reached: sim.reached })
}
