//! 最初の縦断シナリオ: メモリを確保し、小さなファイルを読み、待機し、終了する。
//!
//! ここの `Runtime` は、実行統括と Personality とファイルサービスの役を最小限に演じる。
//! CPU 実行器の代わりに、スレッドが API 境界で止まるたびに次の一手を決める台本を使う。
//! 正常系に加えて、遅延完了、部分結果、失敗、取消との競合 (どちらが勝つ場合も)、
//! 重複通知、遅い通知、読み込み中のプロセス終了、資源不足を通す。
#![cfg(test)]

use kome::{
    Access, AddrWidth, BackingId, BackingOffset, Budget, CancelReply, Census, Completion, CompletionKind, Deadline,
    Duration, Effect, Error, ExitCode, GuestAddr, GuestRange, HostError, InstanceId, Instant, Interruptible, IoId,
    IoLinks, IoOutcome, IoSettled, Kernel, Limits, MapSource, ObjectId, PageSize, ProcessId, Prot, Refusal, Resource,
    SignalMode, SignalSpec, Start, Stop, Tag, ThreadFate, ThreadId, WaitMode, WaitOutcome, WaitStart, WorkerId,
};
use kome_testkit::host::{Arrived, Behavior, CancelPolicy, FakeHost, FileId};

const PAGE: u64 = 0x1000;
const BUFFER: u64 = 0x10000;
const FILE: FileId = FileId(1);
const CONTENTS: &[u8] = b"kome: headless read";
const WORKER: WorkerId = WorkerId::new(0);

struct Runtime {
    k: Kernel,
    host: FakeHost,
    process: ProcessId,
    thread: ThreadId,
    buffer: BackingId,
    done: ObjectId,
    effects: Vec<Effect>,
    /// 読み込みの結果をゲストのメモリへ書いた回数。
    published: usize,
}

impl Runtime {
    fn new(limits: Limits, behavior: Behavior) -> Runtime {
        let host = FakeHost::new(behavior);
        let mut k = Kernel::new(InstanceId::new(7), limits).unwrap();
        let (process, _) = k.create_process(AddrWidth::Bits32, PageSize::new(PAGE).unwrap()).unwrap();
        let (thread, _) = k.create_thread(process, Start::Ready).unwrap();
        // ゲストのメモリを確保する (Personality にとっての VirtualAlloc)。
        let buffer = k.create_backing(process, PAGE).unwrap();
        let range = GuestRange::new(GuestAddr::new(BUFFER), PAGE).unwrap();
        k.map(process, range, MapSource::Backing { backing: buffer, offset: BackingOffset::new(0) }, Prot::READ_WRITE)
            .unwrap();
        let done = k.create_object(process, SignalSpec::new(SignalMode::Consume, 0, 1)).unwrap();
        let mut runtime = Runtime { k, host, process, thread, buffer, done, effects: Vec::new(), published: 0 };
        runtime.host.alloc(buffer, PAGE).unwrap();
        runtime.host.add_file(FILE, CONTENTS);
        runtime.pump();
        runtime
    }

    /// `Effect` をホストへ渡し、記録する。
    fn pump(&mut self) {
        self.k.audit().unwrap();
        while let Some(effect) = self.k.poll_effect() {
            self.host.on_effect(effect, self.k.now()).unwrap();
            self.effects.push(effect);
        }
    }

    /// スレッドを API 境界まで実行させる (CPU 実行器の役)。
    fn run_to_call(&mut self) {
        let permit = self.k.grant(WORKER).unwrap().expect("実行可能なスレッドがある");
        assert_eq!(permit.thread(), self.thread);
        assert_eq!(self.k.relinquish(permit, Stop::Trap, Budget::new(3)).unwrap(), ThreadFate::Servicing);
        self.pump();
    }

    /// 読み込みを始める (手順 2、3)。
    fn start_read(&mut self, len: u64) -> IoId {
        let io = self.k.prepare_io(self.process, IoLinks::NONE.with_notify(self.done)).unwrap();
        self.host.expect_read(io, FILE, 0, len);
        self.k.issue_io(io).unwrap();
        self.pump();
        io
    }

    /// 完了を照合し、出力をゲストへ公開してから通知する (手順 5、6、7)。
    fn deliver(&mut self, arrived: &Arrived) -> Result<IoSettled, Error> {
        let completion = Completion::new(self.k.instance(), CompletionKind::Io { io: arrived.io, outcome: arrived.outcome });
        let settled = self.k.complete(completion)?;
        if let IoSettled::Held(_) = settled {
            // 出力先を基準の経路で引き、内容を書いてから公開する。
            let range = GuestRange::new(GuestAddr::new(BUFFER), PAGE).unwrap();
            let span = self.k.translate(self.process, range, Access::Write).unwrap().unwrap();
            self.host.write(span.backing(), span.offset().get(), &arrived.data).unwrap();
            self.published += 1;
            self.k.publish_io(arrived.io)?;
        }
        self.pump();
        Ok(settled)
    }

    /// 次の出来事 (完了または期限) まで仮想時刻を進め、届いた完了を処理する。
    fn advance_to_next(&mut self) -> Vec<Arrived> {
        let next = [self.host.next_completion(), self.k.next_deadline()].into_iter().flatten().min();
        let to = next.expect("待つものがある");
        let to = to.max(self.k.now());
        while self.k.advance(to).unwrap().more() {}
        self.pump();
        let arrived = self.host.arrivals(self.k.now());
        for arrival in &arrived {
            let _ = self.deliver(arrival);
        }
        arrived
    }

    fn wait_done(&mut self, deadline: Deadline) -> WaitStart {
        let started = self.k.wait(self.thread, &[self.done], WaitMode::Any, deadline, Interruptible::Yes).unwrap();
        self.pump();
        started
    }

    fn woke(&self, outcome: WaitOutcome) -> bool {
        self.effects.contains(&Effect::WaitDone { thread: self.thread, outcome })
    }

    fn buffer(&self) -> &[u8] {
        self.host.bytes(self.buffer).unwrap()
    }

    /// プロセスを終了させ、資源がすべて回収されたことを確かめる。
    fn exit_and_check(mut self) -> Vec<Effect> {
        if self.k.process_phase(self.process).is_ok() {
            let _ = self.k.exit_process(self.process, ExitCode::new(0));
        }
        self.pump();
        while self.host.next_completion().is_some() {
            self.advance_to_next();
        }
        assert_eq!(self.k.census(), Census::default(), "資源が回収されていない");
        assert_eq!(self.host.live_backings(), 0, "ホストの実体が解放されていない");
        self.effects
    }
}

fn delayed(nanos: u64) -> Behavior {
    Behavior { latency: Duration::from_nanos(nanos), ..Behavior::default() }
}

#[test]
fn read_wait_exit_with_an_immediate_completion() {
    let mut rt = Runtime::new(Limits::DEFAULT, Behavior::default());
    rt.run_to_call();
    let io = rt.start_read(64);
    // 完了は同じ時刻に届いている。待機を登録する前に処理される。
    for arrival in rt.host.arrivals(rt.k.now()) {
        assert_eq!(rt.deliver(&arrival), Ok(IoSettled::Held(IoOutcome::Done { transferred: 19 })));
    }
    assert_eq!(rt.wait_done(Deadline::Never), WaitStart::Done(WaitOutcome::Signaled { index: 0 }));
    assert_eq!(&rt.buffer()[..CONTENTS.len()], CONTENTS);
    rt.k.release_io(io).unwrap();
    rt.k.continue_thread(rt.thread).unwrap();
    rt.run_to_call();
    rt.k.terminate_thread(rt.thread, ExitCode::new(0)).unwrap();
    let effects = rt.exit_and_check();
    assert!(effects.iter().any(|e| matches!(e, Effect::ProcessExited { code, .. } if code.get() == 0)));
}

#[test]
fn a_delayed_completion_wakes_the_waiter_after_the_output_is_written() {
    let mut rt = Runtime::new(Limits::DEFAULT, delayed(500));
    rt.run_to_call();
    let io = rt.start_read(64);
    assert_eq!(rt.wait_done(Deadline::Never), WaitStart::Blocked);
    assert!(rt.buffer().iter().all(|byte| *byte == 0));
    let arrived = rt.advance_to_next();
    assert_eq!(arrived.len(), 1);
    assert_eq!(rt.k.now(), Instant::from_nanos(500), "実時間ではなく仮想時刻で待つ");
    assert!(rt.woke(WaitOutcome::Signaled { index: 0 }));
    assert_eq!(&rt.buffer()[..CONTENTS.len()], CONTENTS);
    rt.k.release_io(io).unwrap();
    rt.exit_and_check();
}

#[test]
fn a_partial_result_reports_what_was_transferred() {
    let mut rt = Runtime::new(Limits::DEFAULT, Behavior { chunk: 4, ..delayed(10) });
    rt.run_to_call();
    let io = rt.start_read(64);
    assert_eq!(rt.wait_done(Deadline::Never), WaitStart::Blocked);
    rt.advance_to_next();
    assert_eq!(rt.k.io_outcome(io), Ok(Some(IoOutcome::Done { transferred: 4 })));
    assert_eq!(&rt.buffer()[..6], b"kome\0\0");
    rt.exit_and_check();
}

#[test]
fn a_host_failure_is_an_outcome_not_a_kernel_error() {
    let mut rt = Runtime::new(Limits::DEFAULT, Behavior { fail: Some(HostError::new(5)), ..delayed(10) });
    rt.run_to_call();
    let io = rt.start_read(64);
    assert_eq!(rt.wait_done(Deadline::Never), WaitStart::Blocked);
    rt.advance_to_next();
    assert_eq!(rt.k.io_outcome(io), Ok(Some(IoOutcome::Failed { error: HostError::new(5), transferred: 0 })));
    assert!(rt.woke(WaitOutcome::Signaled { index: 0 }), "失敗でも待機は解除される");
    rt.exit_and_check();
}

#[test]
fn a_wait_times_out_and_the_read_is_cancelled() {
    for (policy, expected) in [
        (CancelPolicy::CancelWins, IoOutcome::Cancelled { transferred: 0 }),
        (CancelPolicy::CompletionWins, IoOutcome::Done { transferred: 19 }),
    ] {
        let mut rt = Runtime::new(Limits::DEFAULT, Behavior { cancel: policy, ..delayed(1000) });
        rt.run_to_call();
        let io = rt.start_read(64);
        assert_eq!(rt.wait_done(Deadline::At(Instant::from_nanos(100))), WaitStart::Blocked);
        assert!(rt.advance_to_next().is_empty());
        assert!(rt.woke(WaitOutcome::TimedOut));
        // 時間切れの後、取消を要求する。終端結果はホストが決める。
        assert_eq!(rt.k.cancel_io(io), Ok(CancelReply::Requested));
        assert_eq!(rt.k.release_io(io), Err(Error::Refused(Refusal::IoInFlight)), "{policy:?}");
        rt.pump();
        let arrived = rt.advance_to_next();
        assert_eq!(arrived.len(), 1);
        assert_eq!(rt.k.io_outcome(io), Ok(Some(expected)), "{policy:?}");
        assert_eq!(rt.published, 1);
        rt.exit_and_check();
    }
}

#[test]
fn duplicate_and_late_completions_are_refused() {
    let mut rt = Runtime::new(Limits::DEFAULT, delayed(10));
    rt.run_to_call();
    let io = rt.start_read(64);
    assert_eq!(rt.wait_done(Deadline::Never), WaitStart::Blocked);
    let arrived = rt.advance_to_next();
    // 重複通知: 終端は一度だけ。出力ももう一度は書かない。
    assert_eq!(rt.deliver(&arrived[0]), Err(Error::Refused(Refusal::IoSettled)));
    assert_eq!(rt.published, 1);
    // 遅い通知: 回収した後に届く。
    rt.k.release_io(io).unwrap();
    assert_eq!(rt.deliver(&arrived[0]), Err(Error::Stale(Tag::Io)));
    assert_eq!(rt.published, 1);
    rt.exit_and_check();
}

#[test]
fn process_exit_during_a_read_drains_before_reclaim() {
    for policy in [CancelPolicy::CancelWins, CancelPolicy::CompletionWins] {
        let mut rt = Runtime::new(Limits::DEFAULT, Behavior { cancel: policy, ..delayed(1000) });
        rt.run_to_call();
        let io = rt.start_read(64);
        assert_eq!(rt.wait_done(Deadline::Never), WaitStart::Blocked);
        rt.k.exit_process(rt.process, ExitCode::new(3)).unwrap();
        rt.pump();
        assert!(rt.effects.contains(&Effect::IoCancel { io }));
        assert!(!rt.effects.contains(&Effect::ProcessReclaimed { process: rt.process }));
        let arrived = rt.advance_to_next();
        assert_eq!(arrived.len(), 1, "{policy:?}");
        assert_eq!(rt.published, 0, "終了したプロセスのメモリへ結果を書いた");
        let effects = rt.exit_and_check();
        assert_eq!(effects.last(), Some(&Effect::ProcessReclaimed { process: effects_process(&effects) }));
    }
}

fn effects_process(effects: &[Effect]) -> ProcessId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::ProcessExited { process, .. } => Some(*process),
            Effect::Interrupt { .. }
            | Effect::WaitDone { .. }
            | Effect::ThreadExited { .. }
            | Effect::ProcessReclaimed { .. }
            | Effect::ObjectReclaimed { .. }
            | Effect::BackingReclaimed { .. }
            | Effect::IoSubmit { .. }
            | Effect::IoCancel { .. } => None,
        })
        .unwrap()
}

#[test]
fn shortage_is_reported_before_the_request_is_accepted() {
    let mut rt = Runtime::new(Limits::DEFAULT.with_ios(1), delayed(10));
    rt.run_to_call();
    let io = rt.start_read(64);
    let before = rt.k.snapshot().unwrap();
    assert_eq!(rt.k.prepare_io(rt.process, IoLinks::NONE), Err(Error::Exhausted(Resource::Ios)));
    assert_eq!(rt.k, before, "受理しなかった要求が状態を変えた");
    assert_eq!(rt.wait_done(Deadline::Never), WaitStart::Blocked);
    rt.advance_to_next();
    rt.k.release_io(io).unwrap();
    // 空いたので、次の要求は受理される。
    let again = rt.start_read(4);
    assert_ne!(again, io, "回収した要求の ID を再利用した");
    rt.exit_and_check();
}
