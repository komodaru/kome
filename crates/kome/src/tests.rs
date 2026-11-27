//! カーネル全体のシナリオ試験と、検査 (`audit`) が違反を検出できることの試験。
//!
//! どの試験も、操作のたびに `audit` を通す (`ok` と `drain`)。

use crate::thread::Run;
use crate::*;

const W0: WorkerId = WorkerId::new(0);
const W1: WorkerId = WorkerId::new(1);
const PAGE: u64 = 0x1000;

fn kernel_with(limits: Limits) -> Kernel {
    Kernel::new(InstanceId::new(1), limits).unwrap()
}

fn kernel() -> Kernel {
    kernel_with(Limits::DEFAULT.with_workers(2))
}

/// 繰り返しの回数。Miri は一歩ごとの検査が千倍ほど遅いので、同じ経路を少ない回数で通す。
/// 回数そのものが意味を持つ再現 (数千回で増え続けること) は、通常の試験が全回数で確かめる。
fn rounds(full: usize) -> usize {
    if cfg!(miri) { full.min(48) } else { full }
}

/// 操作を行い、検査を通してから結果を返す。
macro_rules! ok {
    ($kernel:expr, $result:expr) => {{
        let result = $result;
        $kernel.audit().unwrap();
        result.unwrap()
    }};
}

fn drain(kernel: &mut Kernel) -> Vec<Effect> {
    kernel.audit().unwrap();
    core::iter::from_fn(|| kernel.poll_effect()).collect()
}

fn process(kernel: &mut Kernel) -> (ProcessId, ObjectId) {
    let made = kernel.create_process(AddrWidth::Bits32, PageSize::new(PAGE).unwrap());
    ok!(kernel, made)
}

fn thread(kernel: &mut Kernel, process: ProcessId) -> (ThreadId, ObjectId) {
    let made = kernel.create_thread(process, Start::Ready);
    ok!(kernel, made)
}

/// 先頭の実行可能なスレッドを一度実行させ、サービス中にして返す。
fn trap(kernel: &mut Kernel, worker: WorkerId) -> ThreadId {
    let permit = ok!(kernel, kernel.grant(worker)).unwrap();
    let thread = permit.thread();
    assert_eq!(ok!(kernel, kernel.relinquish(permit, Stop::Trap, Budget::new(1))), ThreadFate::Servicing);
    thread
}

fn event(kernel: &mut Kernel, process: ProcessId) -> ObjectId {
    let made = kernel.create_object(process, SignalSpec::new(SignalMode::Keep, 0, 1));
    ok!(kernel, made)
}

fn semaphore(kernel: &mut Kernel, process: ProcessId, initial: u64, max: u64) -> ObjectId {
    let made = kernel.create_object(process, SignalSpec::new(SignalMode::Consume, initial, max));
    ok!(kernel, made)
}

fn wait_any(kernel: &mut Kernel, thread: ThreadId, targets: &[ObjectId]) -> WaitStart {
    let started = kernel.wait(thread, targets, WaitMode::Any, Deadline::Never, Interruptible::No);
    ok!(kernel, started)
}

fn range(start: u64, len: u64) -> GuestRange {
    GuestRange::new(GuestAddr::new(start), len).unwrap()
}

fn at(nanos: u64) -> Instant {
    Instant::from_nanos(nanos)
}

fn is_empty(kernel: &Kernel) -> bool {
    kernel.processes.len() == 0
        && kernel.threads.len() == 0
        && kernel.objects.len() == 0
        && kernel.backings.len() == 0
        && kernel.timers.len() == 0
        && kernel.ios.len() == 0
        && kernel.effects.promised() == 0
        && kernel.ready.len() == 0
        && kernel.running.is_empty()
        && kernel.due.is_empty()
        && kernel.backing_bytes == 0
}

// ---- プロセスとスレッド ----

#[test]
fn process_is_created_run_exited_and_reclaimed() {
    let mut k = kernel();
    let (p, p_exit) = process(&mut k);
    let (t, t_exit) = thread(&mut k, p);
    assert_eq!(trap(&mut k, W0), t);
    ok!(k, k.terminate_thread(t, ExitCode::new(7)));
    assert_eq!(
        drain(&mut k),
        vec![
            Effect::ObjectReclaimed { object: t_exit },
            Effect::ThreadExited { thread: t, process: p, code: ExitCode::new(7) },
            Effect::ObjectReclaimed { object: p_exit },
            Effect::ProcessExited { process: p, code: ExitCode::new(7) },
            Effect::ProcessReclaimed { process: p },
        ]
    );
    assert!(is_empty(&k), "資源が回収されていない");
    assert_eq!(k.thread_phase(t), Err(Error::Stale(Tag::Thread)));
    assert_eq!(k.process_phase(p), Err(Error::Stale(Tag::Process)));
}

#[test]
fn exit_signal_outlives_the_thread_while_someone_holds_it() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (watcher, _) = thread(&mut k, p);
    let (worker, worker_exit) = thread(&mut k, p);
    ok!(k, k.retain_object(p, worker_exit));
    assert_eq!(trap(&mut k, W0), watcher);
    assert_eq!(wait_any(&mut k, watcher, &[worker_exit]), WaitStart::Blocked);
    ok!(k, k.terminate_thread(worker, ExitCode::new(3)));
    assert_eq!(
        drain(&mut k),
        vec![
            Effect::WaitDone { thread: watcher, outcome: WaitOutcome::Signaled { index: 0 } },
            Effect::ThreadExited { thread: worker, process: p, code: ExitCode::new(3) },
        ]
    );
    assert_eq!(k.signal_count(worker_exit), Ok(1));
    ok!(k, k.release_object(p, worker_exit));
    assert_eq!(drain(&mut k), vec![Effect::ObjectReclaimed { object: worker_exit }]);
}

#[test]
fn exit_process_waits_for_running_threads_to_stop() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (running, _) = thread(&mut k, p);
    let (idle, _) = thread(&mut k, p);
    let permit = ok!(k, k.grant(W0)).unwrap();
    assert_eq!(permit.thread(), running);
    ok!(k, k.exit_process(p, ExitCode::new(9)));
    assert_eq!(k.process_phase(p), Ok(ProcessPhase::Exiting));
    assert_eq!(k.thread_phase(idle), Err(Error::Stale(Tag::Thread)));
    let effects = drain(&mut k);
    assert!(effects.contains(&Effect::Interrupt { worker: W0, thread: running, run: permit.run() }));
    assert!(!effects.iter().any(|e| matches!(e, Effect::ProcessExited { .. })), "実行中のスレッドが残るのに終了を通知した");
    assert_eq!(k.create_thread(p, Start::Ready), Err(Error::Refused(Refusal::ProcessExiting)));
    assert_eq!(k.exit_process(p, ExitCode::new(1)), Err(Error::Refused(Refusal::ProcessExiting)));
    assert_eq!(ok!(k, k.relinquish(permit, Stop::Yielded, Budget::new(5))), ThreadFate::Exited(ExitCode::new(9)));
    let effects = drain(&mut k);
    assert!(effects.contains(&Effect::ProcessExited { process: p, code: ExitCode::new(9) }));
    assert!(is_empty(&k));
}

#[test]
fn threads_run_in_fifo_order_and_a_preempted_thread_goes_to_the_back() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (a, _) = thread(&mut k, p);
    let (b, _) = thread(&mut k, p);
    let first = ok!(k, k.grant(W0)).unwrap();
    assert_eq!(first.thread(), a);
    assert_eq!(first.budget(), Limits::DEFAULT.quantum());
    assert_eq!(ok!(k, k.relinquish(first, Stop::Yielded, Budget::new(100_003))), ThreadFate::Ready);
    assert_eq!(k.executed(a), Ok(100_003), "区切りの検査で貸した量を超えた実行量を数える");
    let second = ok!(k, k.grant(W0)).unwrap();
    assert_eq!(second.thread(), b);
    let third = ok!(k, k.grant(W1)).unwrap();
    assert_eq!((third.thread(), third.run().get()), (a, 2));
    assert_eq!(k.grant(WorkerId::new(2)), Err(Error::Exhausted(Resource::Workers)));
    let _ = k.relinquish(second, Stop::Trap, Budget::new(0));
    let _ = k.relinquish(third, Stop::Trap, Budget::new(0));
    k.audit().unwrap();
}

#[test]
fn a_worker_holds_one_permit_and_a_thread_is_lent_once() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (_only, _) = thread(&mut k, p);
    let permit = ok!(k, k.grant(W0)).unwrap();
    assert_eq!(k.grant(W0), Err(Error::Refused(Refusal::WorkerBusy)));
    assert_eq!(ok!(k, k.grant(W1)), None, "実行中のスレッドをもう一度貸した");
    let _ = k.relinquish(permit, Stop::Trap, Budget::new(0));
}

#[test]
fn a_stale_or_foreign_permit_changes_nothing() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (_t, _) = thread(&mut k, p);
    // 同じ識別を名乗る別の状態 (識別を一意に選ぶ約束を、呼び出し側が破った場合)。
    let mut twin = kernel();
    assert_eq!(twin.instance(), k.instance());
    let (tp, _) = process(&mut twin);
    let _ = thread(&mut twin, tp);
    let first = ok!(twin, twin.grant(W0)).unwrap();
    assert_eq!(ok!(twin, twin.relinquish(first, Stop::Yielded, Budget::new(1))), ThreadFate::Ready);
    let second = ok!(twin, twin.grant(W0)).unwrap();
    // k が貸していないときに届いた実行権。
    let before = k.snapshot().unwrap();
    let idle = ok!(twin, twin.relinquish(second, Stop::Yielded, Budget::new(1)));
    assert_eq!(idle, ThreadFate::Ready);
    let third = ok!(twin, twin.grant(W0)).unwrap();
    assert_eq!(k.relinquish(third, Stop::Trap, Budget::new(1)).map_err(|refused| refused.error()), Err(Error::Refused(Refusal::StalePermit)));
    assert_eq!(k, before);
    // k は 1 回目の実行を貸しているので、4 回目の実行の実行権は古い。
    let mine = ok!(k, k.grant(W0)).unwrap();
    let fourth = {
        let again = ok!(twin, twin.grant(W1));
        assert_eq!(again, None, "実行中のスレッドをもう一度貸した");
        let mut late = kernel();
        let (lp, _) = process(&mut late);
        let _ = thread(&mut late, lp);
        for _ in 0..3 {
            let permit = ok!(late, late.grant(W0)).unwrap();
            assert_eq!(ok!(late, late.relinquish(permit, Stop::Yielded, Budget::new(1))), ThreadFate::Ready);
        }
        ok!(late, late.grant(W0)).unwrap()
    };
    let before = k.snapshot().unwrap();
    assert_eq!(k.relinquish(fourth, Stop::Trap, Budget::new(1)).map_err(|refused| refused.error()), Err(Error::Refused(Refusal::StalePermit)));
    assert_eq!(k, before);
    // 別の worker が同じ回の実行権を名乗っても受け取らない。
    let other_worker = {
        let mut late = kernel();
        let (lp, _) = process(&mut late);
        let _ = thread(&mut late, lp);
        ok!(late, late.grant(W1)).unwrap()
    };
    assert_eq!(k.relinquish(other_worker, Stop::Trap, Budget::new(1)).map_err(|refused| refused.error()), Err(Error::Refused(Refusal::StalePermit)));
    assert_eq!(k, before);
    let mut other = Kernel::new(InstanceId::new(2), Limits::DEFAULT).unwrap();
    let (op, _) = process(&mut other);
    let _ = thread(&mut other, op);
    let foreign = other.grant(W0).unwrap().unwrap();
    assert_eq!(k.relinquish(foreign, Stop::Trap, Budget::new(1)).map_err(|refused| refused.error()), Err(Error::Foreign));
    assert_eq!(k, before);
    assert_eq!(ok!(k, k.relinquish(mine, Stop::Trap, Budget::new(1))), ThreadFate::Servicing);
}

#[test]
fn a_snapshot_is_restored_only_as_another_instance_with_nothing_lent() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    let quiet = k.snapshot().unwrap();
    assert_eq!(quiet.instance(), k.instance());
    assert_eq!(quiet.restore(k.instance()).err(), Some(Error::Refused(Refusal::SameInstance)));
    let permit = ok!(k, k.grant(W0)).unwrap();
    let lent = k.snapshot().unwrap();
    assert_eq!(lent.restore(InstanceId::new(7)).err(), Some(Error::Refused(Refusal::PermitsLent)));
    // 復元した状態は、元の実行インスタンスの実行権を受け取らない。
    let mut restored = quiet.restore(InstanceId::new(7)).unwrap();
    assert_eq!(restored.instance(), InstanceId::new(7));
    let before = restored.snapshot().unwrap();
    assert_eq!(restored.relinquish(permit, Stop::Trap, Budget::new(1)).map_err(|refused| refused.error()), Err(Error::Foreign));
    assert_eq!(restored, before);
    // 発行済みの I/O があると復元できない。完了通知は元の実行インスタンス宛てだからである。
    let own = ok!(restored, restored.grant(W0)).unwrap();
    assert_eq!(own.thread(), t);
    assert_eq!(ok!(restored, restored.relinquish(own, Stop::Trap, Budget::new(1))), ThreadFate::Servicing);
    let io = ok!(restored, restored.prepare_io(p, IoLinks::NONE));
    let prepared = restored.snapshot().unwrap();
    assert!(prepared.restore(InstanceId::new(8)).is_ok());
    ok!(restored, restored.issue_io(io));
    let issued = restored.snapshot().unwrap();
    assert_eq!(issued.restore(InstanceId::new(8)).err(), Some(Error::Refused(Refusal::IoInFlight)));
    ok!(restored, restored.cancel_io(io));
    let cancelling = restored.snapshot().unwrap();
    assert_eq!(cancelling.restore(InstanceId::new(8)).err(), Some(Error::Refused(Refusal::IoInFlight)));
    let done = Completion::new(restored.instance(), CompletionKind::Io { io, outcome: IoOutcome::Cancelled { transferred: 0 } });
    ok!(restored, restored.complete(done));
    let settled = restored.snapshot().unwrap();
    let mut third = settled.restore(InstanceId::new(8)).unwrap();
    assert_eq!(third.complete(done), Err(Error::Foreign));
    assert!(third.audit().is_ok());
}

#[test]
fn a_restored_state_behaves_like_the_original() {
    // 同じ操作を、元の状態と復元した状態に与える。返答も Effect も状態も同じになる
    // (確保済みの容量も写るので、復元した状態でも実行権の貸し出しと Effect の発行が通る)。
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (_t, _) = thread(&mut k, p);
    let object = ok!(k, k.create_object(p, SignalSpec::new(SignalMode::Keep, 0, 1)));
    let snapshot = k.snapshot().unwrap();
    let mut restored = snapshot.restore(InstanceId::new(9)).unwrap();
    for kernel in [&mut k, &mut restored] {
        let permit = kernel.grant(W0).unwrap().unwrap();
        assert_eq!(kernel.relinquish(permit, Stop::Trap, Budget::new(5)).ok(), Some(ThreadFate::Servicing));
        let (q, _) = process(kernel);
        let _ = thread(kernel, q);
        assert!(kernel.signal(object, SignalOp::Set(1)).is_ok());
        assert!(kernel.exit_process(p, ExitCode::new(3)).is_ok());
        assert!(kernel.audit().is_ok());
    }
    assert_eq!(k.census(), restored.census());
    assert_eq!(k.pending_effects(), restored.pending_effects());
    assert!(k.pending_effects() > 0);
    for at in 0..k.pending_effects() {
        assert_eq!(k.peek_effect(at), restored.peek_effect(at));
    }
    assert_eq!(k.peek_effect(k.pending_effects()), None);
    while let Some(effect) = k.poll_effect() {
        assert_eq!(restored.poll_effect(), Some(effect));
    }
    // 実行インスタンスと、復元の後に作った資源の発行元が違う。
    assert_ne!(k, restored);
    // 祖先の識別へは戻せない (祖先が発行した ID と見分けが付かなくなる)。
    let again = restored.snapshot().unwrap();
    assert_eq!(again.restore(k.instance()).err(), Some(Error::Refused(Refusal::SameInstance)));
    assert_eq!(again.restore(InstanceId::new(10)).map(|third| third.census()), Ok(restored.census()));
}

#[test]
fn suspension_combines_with_every_run_state() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (a, _) = thread(&mut k, p);
    let made = k.create_thread(p, Start::Suspended);
    let (b, _) = ok!(k, made);
    // 実行待ち + 停止: 貸さない。
    assert_eq!(ok!(k, k.suspend_thread(a)), 0);
    assert_eq!(ok!(k, k.grant(W0)), None);
    assert_eq!(ok!(k, k.unsuspend_thread(a)), 1);
    assert_eq!(k.unsuspend_thread(a), Err(Error::Refused(Refusal::NotSuspended)));
    // 実行中 + 停止: 中断を一度だけ要求し、返ってきたら貸さない。
    let permit = ok!(k, k.grant(W0)).unwrap();
    assert_eq!(ok!(k, k.suspend_thread(a)), 0);
    assert_eq!(ok!(k, k.suspend_thread(a)), 1);
    assert_eq!(drain(&mut k), vec![Effect::Interrupt { worker: W0, thread: a, run: permit.run() }]);
    assert_eq!(ok!(k, k.relinquish(permit, Stop::Yielded, Budget::new(1))), ThreadFate::Ready);
    assert_eq!(ok!(k, k.grant(W0)), None);
    assert_eq!(ok!(k, k.unsuspend_thread(a)), 2);
    assert_eq!(ok!(k, k.unsuspend_thread(a)), 1);
    // 待機中 + 停止: 待機は確定するが、停止が解けるまで貸さない。
    let e = event(&mut k, p);
    assert_eq!(trap(&mut k, W0), a);
    assert_eq!(wait_any(&mut k, a, &[e]), WaitStart::Blocked);
    assert_eq!(ok!(k, k.suspend_thread(a)), 0);
    assert_eq!(ok!(k, k.signal(e, SignalOp::Set(1))), 0);
    assert_eq!(drain(&mut k), vec![Effect::WaitDone { thread: a, outcome: WaitOutcome::Signaled { index: 0 } }]);
    ok!(k, k.continue_thread(a));
    assert_eq!(ok!(k, k.grant(W0)), None);
    assert_eq!(ok!(k, k.unsuspend_thread(a)), 1);
    assert_eq!(ok!(k, k.unsuspend_thread(b)), 1);
    assert_eq!(ok!(k, k.grant(W0)).map(|permit| permit.thread()), Some(a));
    assert_eq!(k.suspend_count(b), Ok(0));
}

#[test]
fn continue_requires_a_servicing_thread() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    assert_eq!(k.continue_thread(t), Err(Error::Refused(Refusal::NotServicing)));
    assert_eq!(trap(&mut k, W0), t);
    ok!(k, k.continue_thread(t));
    assert_eq!(k.thread_phase(t), Ok(ThreadPhase::Ready));
    assert_eq!(k.process_of(t), Ok(p));
    assert_eq!(k.thread_count(p), Ok(1));
}

// ---- 資源と待機 ----

#[test]
fn a_satisfied_wait_does_not_register() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    let sem = semaphore(&mut k, p, 1, 5);
    let ev = event(&mut k, p);
    ok!(k, k.signal(ev, SignalOp::Set(1)));
    assert_eq!(trap(&mut k, W0), t);
    assert_eq!(wait_any(&mut k, t, &[sem]), WaitStart::Done(WaitOutcome::Signaled { index: 0 }));
    assert_eq!(k.signal_count(sem), Ok(0), "消費する資源の数が減っていない");
    assert_eq!(wait_any(&mut k, t, &[sem, ev]), WaitStart::Done(WaitOutcome::Signaled { index: 1 }));
    assert_eq!(k.signal_count(ev), Ok(1), "消費しない資源の数が変わった");
    assert_eq!(k.thread_phase(t), Ok(ThreadPhase::Servicing));
    assert!(drain(&mut k).is_empty());
}

#[test]
fn waiters_are_woken_in_registration_order() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let threads: Vec<ThreadId> = (0..3).map(|_| thread(&mut k, p).0).collect();
    let sem = semaphore(&mut k, p, 0, 10);
    for t in &threads {
        assert_eq!(trap(&mut k, W0), *t);
        assert_eq!(wait_any(&mut k, *t, &[sem]), WaitStart::Blocked);
    }
    assert_eq!(ok!(k, k.signal(sem, SignalOp::Add(2))), 0);
    let expected: Vec<Effect> = threads[..2]
        .iter()
        .map(|thread| Effect::WaitDone { thread: *thread, outcome: WaitOutcome::Signaled { index: 0 } })
        .collect();
    assert_eq!(drain(&mut k), expected);
    assert_eq!(k.signal_count(sem), Ok(0));
    assert_eq!(k.thread_phase(threads[2]), Ok(ThreadPhase::Blocked));
    assert_eq!(k.signal(sem, SignalOp::Add(11)), Err(Error::Refused(Refusal::SignalOverflow)));
    assert_eq!(k.signal(sem, SignalOp::Set(11)), Err(Error::Refused(Refusal::SignalOverflow)));
}

#[test]
fn wait_all_takes_everything_at_once_or_nothing() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (all, _) = thread(&mut k, p);
    let (any, _) = thread(&mut k, p);
    let a = semaphore(&mut k, p, 0, 1);
    let b = semaphore(&mut k, p, 0, 1);
    assert_eq!(trap(&mut k, W0), all);
    let started = k.wait(all, &[a, b], WaitMode::All, Deadline::Never, Interruptible::No);
    assert_eq!(ok!(k, started), WaitStart::Blocked);
    assert_eq!(trap(&mut k, W0), any);
    assert_eq!(wait_any(&mut k, any, &[a]), WaitStart::Blocked);
    // a だけが成立: 「すべて」の待機は a を先に取らない。後から登録した待機が a を取る。
    ok!(k, k.signal(a, SignalOp::Add(1)));
    assert_eq!(drain(&mut k), vec![Effect::WaitDone { thread: any, outcome: WaitOutcome::Signaled { index: 0 } }]);
    assert_eq!(k.signal_count(a), Ok(0));
    ok!(k, k.signal(b, SignalOp::Add(1)));
    assert!(drain(&mut k).is_empty());
    assert_eq!(k.signal_count(b), Ok(1), "「すべて」の待機が一部だけを消費した");
    ok!(k, k.signal(a, SignalOp::Add(1)));
    assert_eq!(drain(&mut k), vec![Effect::WaitDone { thread: all, outcome: WaitOutcome::AllSignaled }]);
    assert_eq!((k.signal_count(a), k.signal_count(b)), (Ok(0), Ok(0)));
}

#[test]
fn wait_validates_before_changing_anything() {
    let mut k = kernel_with(Limits::DEFAULT.with_wait_targets(2));
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    let a = event(&mut k, p);
    let b = event(&mut k, p);
    let c = event(&mut k, p);
    assert_eq!(
        k.wait(t, &[a], WaitMode::Any, Deadline::Never, Interruptible::No),
        Err(Error::Refused(Refusal::NotServicing))
    );
    assert_eq!(trap(&mut k, W0), t);
    let before = k.snapshot().unwrap();
    let refused = [
        (vec![a, a], Error::Refused(Refusal::DuplicateTarget)),
        (vec![a, b, c], Error::Exhausted(Resource::WaitTargets)),
    ];
    for (targets, error) in refused {
        assert_eq!(k.wait(t, &targets, WaitMode::Any, Deadline::Never, Interruptible::No), Err(error));
        assert_eq!(k, before);
    }
    ok!(k, k.release_object(p, c));
    assert_eq!(k.wait(t, &[c], WaitMode::Any, Deadline::Never, Interruptible::No), Err(Error::Stale(Tag::Object)));
}

#[test]
fn a_closed_object_lives_until_its_waiters_leave() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    let e = event(&mut k, p);
    assert_eq!(trap(&mut k, W0), t);
    let started = k.wait(t, &[e], WaitMode::Any, Deadline::Never, Interruptible::Yes);
    assert_eq!(ok!(k, started), WaitStart::Blocked);
    ok!(k, k.release_object(p, e));
    assert_eq!(k.release_object(p, e), Err(Error::Refused(Refusal::NotHeld)));
    assert!(drain(&mut k).is_empty(), "待機が残っているのに資源を破棄した");
    ok!(k, k.interrupt_wait(t));
    assert_eq!(drain(&mut k), vec![Effect::ObjectReclaimed { object: e }]);
    assert_eq!(k.thread_phase(t), Ok(ThreadPhase::Servicing));
    assert_eq!(k.interrupt_wait(t), Err(Error::Refused(Refusal::NotWaiting)));
}

#[test]
fn only_interruptible_waits_are_interrupted() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    let e = event(&mut k, p);
    assert_eq!(trap(&mut k, W0), t);
    assert_eq!(wait_any(&mut k, t, &[e]), WaitStart::Blocked);
    assert_eq!(k.interrupt_wait(t), Err(Error::Refused(Refusal::NotInterruptible)));
    assert_eq!(k.thread_phase(t), Ok(ThreadPhase::Blocked));
}

#[test]
fn terminating_a_blocked_thread_removes_its_wait() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (keeper, _) = thread(&mut k, p);
    let (t, _) = thread(&mut k, p);
    let e = event(&mut k, p);
    let _ = keeper;
    assert_eq!(trap(&mut k, W0), keeper);
    assert_eq!(trap(&mut k, W0), t);
    let started = k.wait(t, &[e], WaitMode::Any, Deadline::At(at(50)), Interruptible::No);
    assert_eq!(ok!(k, started), WaitStart::Blocked);
    ok!(k, k.terminate_thread(t, ExitCode::new(0)));
    assert_eq!(k.next_deadline(), None);
    ok!(k, k.signal(e, SignalOp::Set(1)));
    let effects = drain(&mut k);
    assert!(!effects.iter().any(|e| matches!(e, Effect::WaitDone { .. })), "終了したスレッドの待機が確定した");
}

#[test]
fn spec_must_be_consistent() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    for spec in [SignalSpec::new(SignalMode::Keep, 0, 0), SignalSpec::new(SignalMode::Consume, 3, 2)] {
        assert_eq!(k.create_object(p, spec), Err(Error::Refused(Refusal::BadSignalSpec)));
    }
}

// ---- 時刻とタイマ ----

#[test]
fn a_wait_times_out_exactly_once_at_its_deadline() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    let e = event(&mut k, p);
    assert_eq!(trap(&mut k, W0), t);
    let poll = k.wait(t, &[e], WaitMode::Any, Deadline::At(at(0)), Interruptible::No);
    assert_eq!(ok!(k, poll), WaitStart::Done(WaitOutcome::TimedOut));
    let started = k.wait(t, &[e], WaitMode::Any, Deadline::At(at(100)), Interruptible::No);
    assert_eq!(ok!(k, started), WaitStart::Blocked);
    assert_eq!(k.next_deadline(), Some(at(100)));
    assert_eq!(ok!(k, k.advance(at(99))).fired(), 0);
    assert_eq!(ok!(k, k.advance(at(100))).fired(), 1);
    assert_eq!(drain(&mut k), vec![Effect::WaitDone { thread: t, outcome: WaitOutcome::TimedOut }]);
    // 時間切れの後に来た通知は、確定済みの待機を変えない。
    ok!(k, k.signal(e, SignalOp::Set(1)));
    assert!(drain(&mut k).is_empty());
    assert_eq!(k.advance(at(99)), Err(Error::Refused(Refusal::TimeReversed)));
    assert_eq!(k.now(), at(100));
}

#[test]
fn a_pure_sleep_ends_only_by_time() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    assert_eq!(trap(&mut k, W0), t);
    for mode in [WaitMode::Any, WaitMode::All] {
        let started = k.wait(t, &[], mode, Deadline::At(at(k.now().nanos() + 10)), Interruptible::No);
        assert_eq!(ok!(k, started), WaitStart::Blocked, "対象の無い待機が成立した");
        let to = k.next_deadline().unwrap();
        assert_eq!(ok!(k, k.advance(to)).fired(), 1);
        assert_eq!(drain(&mut k), vec![Effect::WaitDone { thread: t, outcome: WaitOutcome::TimedOut }]);
    }
}

#[test]
fn same_instant_deadlines_fire_in_registration_order() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (first, _) = thread(&mut k, p);
    let (second, _) = thread(&mut k, p);
    let sem = semaphore(&mut k, p, 0, 1);
    // 1 番目: 時刻 10 に時間切れ。2 番目: 時刻 10 に sem を増やすタイマ。3 番目: sem を待ち、時刻 10 に時間切れ。
    assert_eq!(trap(&mut k, W0), first);
    let started = k.wait(first, &[sem], WaitMode::Any, Deadline::At(at(10)), Interruptible::No);
    assert_eq!(ok!(k, started), WaitStart::Blocked);
    let timer = ok!(k, k.create_timer(p, sem));
    ok!(k, k.arm_timer(timer, at(10), None));
    assert_eq!(trap(&mut k, W0), second);
    let started = k.wait(second, &[sem], WaitMode::Any, Deadline::At(at(10)), Interruptible::No);
    assert_eq!(ok!(k, started), WaitStart::Blocked);
    assert_eq!(ok!(k, k.advance(at(10))).fired(), 2, "タイマで起きた待機の期限まで処理した");
    assert_eq!(
        drain(&mut k),
        vec![
            Effect::WaitDone { thread: first, outcome: WaitOutcome::TimedOut },
            Effect::WaitDone { thread: second, outcome: WaitOutcome::Signaled { index: 0 } },
        ]
    );
}

#[test]
fn periodic_timer_refires_and_stops_when_disarmed() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let sem = semaphore(&mut k, p, 0, 100);
    let timer = ok!(k, k.create_timer(p, sem));
    let min = Limits::DEFAULT.min_period();
    let period = min.get();
    assert_eq!(
        k.arm_timer(timer, at(5), Period::new(Duration::from_nanos(period.nanos() - 1))),
        Err(Error::Refused(Refusal::PeriodTooShort))
    );
    ok!(k, k.arm_timer(timer, at(5), Some(min)));
    assert_eq!(ok!(k, k.advance(at(5 + 2 * period.nanos()))).fired(), 3);
    assert_eq!(k.signal_count(sem), Ok(3));
    assert_eq!(k.next_deadline(), Some(at(5 + 3 * period.nanos())));
    // 置き換え: 古い期限は残らない。
    ok!(k, k.arm_timer(timer, at(k.now().nanos() + 1), None));
    assert_eq!(k.next_deadline(), Some(at(k.now().nanos() + 1)));
    assert!(ok!(k, k.disarm_timer(timer)));
    assert!(!ok!(k, k.disarm_timer(timer)));
    assert_eq!(k.next_deadline(), None);
    ok!(k, k.release_object(p, sem));
    assert!(drain(&mut k).is_empty(), "タイマが使っている資源を破棄した");
    ok!(k, k.drop_timer(timer));
    assert_eq!(drain(&mut k), vec![Effect::ObjectReclaimed { object: sem }]);
    assert_eq!(k.drop_timer(timer), Err(Error::Stale(Tag::Timer)));
}

#[test]
fn a_timer_set_in_the_past_fires_at_the_current_time() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let e = event(&mut k, p);
    let timer = ok!(k, k.create_timer(p, e));
    assert!(!ok!(k, k.advance(at(1000))).more());
    ok!(k, k.arm_timer(timer, at(10), None));
    assert_eq!(ok!(k, k.advance(at(1000))).fired(), 1);
    assert_eq!(k.now(), at(1000));
    assert_eq!(k.signal_count(e), Ok(1));
}

#[test]
fn a_period_that_leaves_the_time_range_stops_the_timer() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let e = event(&mut k, p);
    let timer = ok!(k, k.create_timer(p, e));
    ok!(k, k.arm_timer(timer, at(u64::MAX - 5), Some(Limits::DEFAULT.min_period())));
    assert_eq!(ok!(k, k.advance(at(u64::MAX))).fired(), 1);
    assert_eq!(k.next_deadline(), None);
    assert!(!ok!(k, k.disarm_timer(timer)));
}

// ---- I/O ----

/// 準備して発行し、通知先を待つスレッドを用意する。
fn issued_io(k: &mut Kernel) -> (ProcessId, ThreadId, ObjectId, IoId) {
    let (p, _) = process(k);
    let (t, _) = thread(k, p);
    let done = event(k, p);
    let io = ok!(k, k.prepare_io(p, IoLinks::NONE.with_notify(done)));
    ok!(k, k.issue_io(io));
    assert_eq!(drain(k), vec![Effect::IoSubmit { io }]);
    assert_eq!(trap(k, W0), t);
    assert_eq!(wait_any(k, t, &[done]), WaitStart::Blocked);
    (p, t, done, io)
}

fn completion(k: &Kernel, io: IoId, outcome: IoOutcome) -> Completion {
    Completion::new(k.instance(), CompletionKind::Io { io, outcome })
}

const DONE: IoOutcome = IoOutcome::Done { transferred: 12 };

#[test]
fn completion_wakes_the_waiter_only_after_publication() {
    let mut k = kernel();
    let (_p, t, done, io) = issued_io(&mut k);
    assert_eq!(ok!(k, k.complete(completion(&k, io, DONE))), IoSettled::Held(DONE));
    assert_eq!(k.io_phase(io), Ok(IoPhase::Settled));
    assert!(drain(&mut k).is_empty(), "出力を公開する前に待機を解除した");
    assert_eq!(k.signal_count(done), Ok(0));
    assert_eq!(ok!(k, k.publish_io(io)), DONE);
    assert_eq!(drain(&mut k), vec![Effect::WaitDone { thread: t, outcome: WaitOutcome::Signaled { index: 0 } }]);
    assert_eq!(k.publish_io(io), Err(Error::Refused(Refusal::IoNotPublishable)));
    assert_eq!(k.io_outcome(io), Ok(Some(DONE)));
    ok!(k, k.release_io(io));
    assert_eq!(k.io_phase(io), Err(Error::Stale(Tag::Io)));
}

#[test]
fn a_second_or_late_or_foreign_completion_changes_nothing() {
    let mut k = kernel();
    let (_p, _t, _done, io) = issued_io(&mut k);
    let foreign = Completion::new(InstanceId::new(99), CompletionKind::Io { io, outcome: DONE });
    let before = k.snapshot().unwrap();
    assert_eq!(k.complete(foreign), Err(Error::Foreign));
    assert_eq!(k, before);
    ok!(k, k.complete(completion(&k, io, DONE)));
    let before = k.snapshot().unwrap();
    let again = completion(&k, io, IoOutcome::Failed { error: HostError::new(5), transferred: 0 });
    assert_eq!(k.complete(again), Err(Error::Refused(Refusal::IoSettled)), "終端済みの I/O が再度終端した");
    assert_eq!(k, before);
    assert_eq!(k.io_outcome(io), Ok(Some(DONE)));
    ok!(k, k.release_io(io));
    let before = k.snapshot().unwrap();
    assert_eq!(k.complete(completion(&k, io, DONE)), Err(Error::Stale(Tag::Io)));
    assert_eq!(k, before);
}

#[test]
fn cancel_is_a_request_and_the_host_decides_the_outcome() {
    for outcome in [DONE, IoOutcome::Cancelled { transferred: 3 }] {
        let mut k = kernel();
        let (_p, _t, _done, io) = issued_io(&mut k);
        assert_eq!(ok!(k, k.cancel_io(io)), CancelReply::Requested);
        assert_eq!(ok!(k, k.cancel_io(io)), CancelReply::AlreadyRequested);
        assert_eq!(drain(&mut k), vec![Effect::IoCancel { io }], "取消の要求は一度だけ出す");
        assert_eq!(k.io_phase(io), Ok(IoPhase::CancelRequested));
        assert_eq!(k.io_outcome(io), Ok(None));
        assert_eq!(k.release_io(io), Err(Error::Refused(Refusal::IoInFlight)), "取消を要求しただけで回収した");
        assert_eq!(ok!(k, k.complete(completion(&k, io, outcome))), IoSettled::Held(outcome));
        assert_eq!(k.cancel_io(io), Err(Error::Refused(Refusal::IoSettled)));
    }
}

#[test]
fn cancelling_before_issue_never_reaches_the_host() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let io = ok!(k, k.prepare_io(p, IoLinks::NONE));
    assert_eq!(k.complete(completion(&k, io, DONE)), Err(Error::Refused(Refusal::IoNotIssued)));
    assert_eq!(ok!(k, k.cancel_io(io)), CancelReply::Withdrawn);
    assert_eq!(k.io_outcome(io), Ok(Some(IoOutcome::Cancelled { transferred: 0 })));
    assert_eq!(k.issue_io(io), Err(Error::Refused(Refusal::IoSettled)));
    assert!(drain(&mut k).is_empty());
    ok!(k, k.release_io(io));
    let fresh = ok!(k, k.prepare_io(p, IoLinks::NONE));
    ok!(k, k.issue_io(fresh));
    assert_eq!(k.issue_io(fresh), Err(Error::Refused(Refusal::IoAlreadyIssued)));
    assert_eq!(k.release_io(fresh), Err(Error::Refused(Refusal::IoInFlight)));
}

#[test]
fn io_keeps_its_target_and_notify_alive() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let file = event(&mut k, p);
    let both = ok!(k, k.prepare_io(p, IoLinks::NONE.with_target(file).with_notify(file)));
    ok!(k, k.release_object(p, file));
    assert!(drain(&mut k).is_empty(), "未完了 I/O の対象を破棄した");
    ok!(k, k.release_io(both));
    assert_eq!(drain(&mut k), vec![Effect::ObjectReclaimed { object: file }]);
}

#[test]
fn process_exit_drains_in_flight_io_before_reclaim() {
    let mut k = kernel();
    let (p, t, done, io) = issued_io(&mut k);
    let prepared = ok!(k, k.prepare_io(p, IoLinks::NONE));
    ok!(k, k.exit_process(p, ExitCode::new(1)));
    let effects = drain(&mut k);
    assert!(effects.contains(&Effect::IoCancel { io }));
    assert!(effects.contains(&Effect::ProcessExited { process: p, code: ExitCode::new(1) }));
    assert!(!effects.contains(&Effect::ObjectReclaimed { object: done }), "未完了 I/O の通知先を破棄した");
    assert!(!effects.contains(&Effect::ProcessReclaimed { process: p }), "未完了 I/O が残るのに回収した");
    assert_eq!(k.process_phase(p), Ok(ProcessPhase::Draining));
    assert_eq!(k.io_phase(prepared), Err(Error::Stale(Tag::Io)));
    assert_eq!(k.thread_phase(t), Err(Error::Stale(Tag::Thread)));
    assert_eq!(k.prepare_io(p, IoLinks::NONE), Err(Error::Refused(Refusal::ProcessExiting)));
    // 取消を要求しても、正常完了が返ることがある。受け取るゲストはもういない。
    assert_eq!(ok!(k, k.complete(completion(&k, io, DONE))), IoSettled::Orphaned(DONE));
    assert_eq!(
        drain(&mut k),
        vec![Effect::ObjectReclaimed { object: done }, Effect::ProcessReclaimed { process: p }]
    );
    assert!(is_empty(&k));
}

#[test]
fn a_full_effect_queue_refuses_before_accepting() {
    // プロセス 3 席 + スレッド 2 席 + I/O 2 席 = 7 席。
    let mut k = kernel_with(Limits::DEFAULT.with_effects(7));
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    let io = ok!(k, k.prepare_io(p, IoLinks::NONE));
    let before = k.snapshot().unwrap();
    assert_eq!(k.prepare_io(p, IoLinks::NONE), Err(Error::Exhausted(Resource::Effects)));
    assert_eq!(k.create_object(p, SignalSpec::new(SignalMode::Keep, 0, 1)), Err(Error::Exhausted(Resource::Effects)));
    assert_eq!(k.grant(W0), Err(Error::Exhausted(Resource::Effects)), "中断の席が無いのに実行権を貸した");
    assert_eq!(k, before);
    // 受理済みの操作の Effect は、満杯でも出せる。
    ok!(k, k.issue_io(io));
    ok!(k, k.cancel_io(io));
    ok!(k, k.terminate_thread(t, ExitCode::new(0)));
    let effects = drain(&mut k);
    assert_eq!(effects.len(), 6, "{effects:?}");
    assert!(effects.contains(&Effect::IoSubmit { io }) && effects.contains(&Effect::IoCancel { io }));
    assert_eq!(ok!(k, k.complete(completion(&k, io, DONE))), IoSettled::Orphaned(DONE));
    assert_eq!(drain(&mut k), vec![Effect::ProcessReclaimed { process: p }]);
}

// ---- メモリ ----

fn backed(backing: BackingId, offset: u64) -> MapSource {
    MapSource::Backing { backing, offset: BackingOffset::new(offset) }
}

fn read(k: &Kernel, p: ProcessId, start: u64, len: u64) -> Result<Span, MemFault> {
    k.translate(p, range(start, len), Access::Read).unwrap()
}

#[test]
fn translate_follows_mappings_and_reports_the_first_fault() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let a = ok!(k, k.create_backing(p, 4 * PAGE));
    let b = ok!(k, k.create_backing(p, PAGE));
    ok!(k, k.map(p, range(0x10000, 2 * PAGE), backed(a, PAGE), Prot::READ_WRITE));
    ok!(k, k.map(p, range(0x12000, PAGE), backed(b, 0), Prot::READ));
    ok!(k, k.map(p, range(0x13000, PAGE), MapSource::Reserved, Prot::READ_WRITE));
    let span = read(&k, p, 0x10ff8, 0x2000).unwrap();
    assert_eq!((span.backing(), span.offset().get(), span.bytes()), (a, PAGE + 0xff8, 0x1008), "境界をまたぐ区間");
    let span = read(&k, p, 0x12000, 0x2000).unwrap();
    assert_eq!((span.backing(), span.offset().get(), span.bytes()), (b, 0, PAGE));
    let fault = |access, addr| k.translate(p, range(addr, 1), access).unwrap().unwrap_err();
    assert_eq!(fault(Access::Write, 0x12fff).kind(), MemFaultKind::Denied);
    assert_eq!(fault(Access::Execute, 0x10000).kind(), MemFaultKind::Denied);
    assert_eq!(fault(Access::Read, 0x13000).kind(), MemFaultKind::Reserved);
    assert_eq!(fault(Access::Read, 0x14000).kind(), MemFaultKind::Unmapped);
    assert_eq!(fault(Access::Read, 0xffff).addr(), GuestAddr::new(0xffff));
    let info = k.mapping_at(p, GuestAddr::new(0x13800)).unwrap().unwrap();
    assert_eq!((info.range(), info.prot(), info.source()), (range(0x13000, PAGE), Prot::NONE, MapSource::Reserved));
    assert_eq!(k.mapping_at(p, GuestAddr::new(0)), Ok(None));
}

#[test]
fn map_refuses_bad_placements_without_changing_state() {
    let mut k = kernel_with(Limits::DEFAULT.with_mappings(2).with_backing_bytes(8 * PAGE));
    let (p, _) = process(&mut k);
    let b = ok!(k, k.create_backing(p, 2 * PAGE));
    ok!(k, k.map(p, range(0x2000, 2 * PAGE), backed(b, 0), Prot::READ));
    let before = k.snapshot().unwrap();
    let rw = Prot::READ_WRITE;
    let refused = [
        (range(0x3000, PAGE), MapSource::Reserved, Refusal::Overlap),
        (range(0x1000, 2 * PAGE), MapSource::Reserved, Refusal::Overlap),
        (range(0x4800, PAGE), MapSource::Reserved, Refusal::Misaligned),
        (range(0x5000, PAGE + 1), MapSource::Reserved, Refusal::Misaligned),
        (range(0x5000, PAGE), backed(b, 0x800), Refusal::Misaligned),
        (range(0x5000, 2 * PAGE), backed(b, PAGE), Refusal::BeyondBacking),
        (range(0x5000, PAGE), backed(b, 2 * PAGE), Refusal::BeyondBacking),
        (range(0xffff_f000, 2 * PAGE), MapSource::Reserved, Refusal::OutOfSpace),
    ];
    for (place, source, refusal) in refused {
        assert_eq!(k.map(p, place, source, rw), Err(Error::Refused(refusal)), "{place:?}");
        assert_eq!(k, before);
    }
    ok!(k, k.map(p, range(0xffff_f000, PAGE), MapSource::Reserved, rw));
    assert_eq!(k.map(p, range(0x8000, PAGE), MapSource::Reserved, rw), Err(Error::Exhausted(Resource::Mappings)));
    assert_eq!(k.create_backing(p, 0), Err(Error::Refused(Refusal::EmptyBacking)));
    assert_eq!(k.create_backing(p, 6 * PAGE + 1), Err(Error::Exhausted(Resource::BackingBytes)));
    assert_eq!(k.backing_len(b), Ok(2 * PAGE));
}

#[test]
fn unmap_splits_and_keeps_offsets() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let b = ok!(k, k.create_backing(p, 8 * PAGE));
    ok!(k, k.map(p, range(0x10000, 8 * PAGE), backed(b, 0), Prot::READ));
    let generation = k.map_generation(p).unwrap();
    assert_eq!(ok!(k, k.unmap(p, range(0x12000, 2 * PAGE))), 2 * PAGE);
    assert!(k.map_generation(p).unwrap() > generation);
    assert_eq!(k.mapping_count(p), Ok(2));
    assert_eq!(read(&k, p, 0x11fff, 2).unwrap().bytes(), 1);
    assert_eq!(read(&k, p, 0x12000, 1).unwrap_err().kind(), MemFaultKind::Unmapped);
    assert_eq!(read(&k, p, 0x14000, 1).unwrap().offset().get(), 4 * PAGE, "分割した後ろ側の位置がずれた");
    // 端をまたぐ範囲: 左右を削り、間のマッピングは無いまま。
    assert_eq!(ok!(k, k.unmap(p, range(0x11000, 4 * PAGE))), 2 * PAGE);
    assert_eq!(read(&k, p, 0x10000, 0x10000).unwrap().bytes(), PAGE);
    assert_eq!(read(&k, p, 0x15000, 1).unwrap().offset().get(), 5 * PAGE);
    let before = k.snapshot().unwrap();
    assert_eq!(k.unmap(p, range(0x40000, PAGE)), Ok(0));
    assert_eq!(k, before, "何も外さない unmap が状態を変えた");
    assert_eq!(ok!(k, k.unmap(p, range(0, 0x1_0000_0000))), 4 * PAGE);
    assert_eq!(k.mapping_count(p), Ok(0));
    assert!(drain(&mut k).is_empty(), "保持が残る実体を解放した");
    ok!(k, k.release_backing(p, b));
    assert_eq!(drain(&mut k), vec![Effect::BackingReclaimed { backing: b }]);
}

#[test]
fn a_mapped_backing_outlives_its_last_hold() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (q, _) = process(&mut k);
    let b = ok!(k, k.create_backing(p, PAGE));
    ok!(k, k.retain_backing(q, b));
    // 同じ実体を二つの範囲と二つのプロセスへ見せる (別名)。
    ok!(k, k.map(p, range(0x1000, PAGE), backed(b, 0), Prot::READ_WRITE));
    ok!(k, k.map(p, range(0x5000, PAGE), backed(b, 0), Prot::READ));
    ok!(k, k.map(q, range(0x9000, PAGE), backed(b, 0), Prot::READ));
    ok!(k, k.release_backing(p, b));
    ok!(k, k.release_backing(q, b));
    assert_eq!(k.release_backing(q, b), Err(Error::Refused(Refusal::NotHeld)));
    ok!(k, k.unmap(p, range(0x1000, PAGE)));
    ok!(k, k.unmap(q, range(0x9000, PAGE)));
    assert!(drain(&mut k).is_empty(), "マッピングが残る実体を解放した");
    assert_eq!(read(&k, p, 0x5000, 1).unwrap().backing(), b);
    ok!(k, k.unmap(p, range(0x5000, PAGE)));
    assert_eq!(drain(&mut k), vec![Effect::BackingReclaimed { backing: b }]);
    assert_eq!(k.backing_len(b), Err(Error::Stale(Tag::Backing)));
}

#[test]
fn protect_needs_full_coverage_and_splits_at_both_ends() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let b = ok!(k, k.create_backing(p, 8 * PAGE));
    ok!(k, k.map(p, range(0x10000, 4 * PAGE), backed(b, 0), Prot::READ_WRITE));
    ok!(k, k.map(p, range(0x14000, PAGE), backed(b, 4 * PAGE), Prot::READ_WRITE));
    ok!(k, k.map(p, range(0x16000, PAGE), MapSource::Reserved, Prot::NONE));
    let before = k.snapshot().unwrap();
    assert_eq!(k.protect(p, range(0x14000, 2 * PAGE), Prot::READ), Err(Error::Refused(Refusal::NotMapped)));
    assert_eq!(k.protect(p, range(0xf000, 2 * PAGE), Prot::READ), Err(Error::Refused(Refusal::NotMapped)));
    assert_eq!(k.protect(p, range(0x16000, PAGE), Prot::READ), Err(Error::Refused(Refusal::NotBacked)));
    assert_eq!(k, before);
    // 一つのマッピングの内側: 三つに分かれる。
    ok!(k, k.protect(p, range(0x11000, PAGE), Prot::READ));
    assert_eq!(k.mapping_count(p), Ok(5));
    let write = |addr| k.translate(p, range(addr, 1), Access::Write).unwrap();
    assert!(write(0x10fff).is_ok() && write(0x12000).is_ok());
    assert_eq!(write(0x11000).unwrap_err().kind(), MemFaultKind::Denied);
    assert_eq!(read(&k, p, 0x11000, 1).unwrap().offset().get(), PAGE);
    assert_eq!(read(&k, p, 0x12000, 1).unwrap().offset().get(), 2 * PAGE);
    // 二つのマッピングにまたがる範囲。
    // 変更の後、二つの部分は同じ権限で同じ実体の続きなので、一つの項目になる。
    ok!(k, k.protect(p, range(0x13000, 2 * PAGE), Prot::NONE));
    assert_eq!(k.mapping_count(p), Ok(5));
    assert_eq!(k.mapping_at(p, GuestAddr::new(0x14000)).unwrap().map(|m| m.range()), Some(range(0x13000, 2 * PAGE)));
    assert_eq!(read(&k, p, 0x12000, 3 * PAGE).unwrap().bytes(), PAGE);
    assert_eq!(read(&k, p, 0x13000, 1).unwrap_err().kind(), MemFaultKind::Denied);
    assert_eq!(read(&k, p, 0x14fff, 1).unwrap_err().kind(), MemFaultKind::Denied);
    assert!(read(&k, p, 0x12fff, 1).is_ok());
}

#[test]
fn find_free_returns_the_lowest_aligned_gap() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    ok!(k, k.map(p, range(0x10000, PAGE), MapSource::Reserved, Prot::NONE));
    ok!(k, k.map(p, range(0x12000, PAGE), MapSource::Reserved, Prot::NONE));
    let page = PageSize::new(PAGE).unwrap();
    let big = PageSize::new(0x10000).unwrap();
    let all = range(0x10000, 0xffff_0000);
    let find = |len, align, within| k.find_free(p, len, align, within).unwrap().map(GuestAddr::get);
    assert_eq!(find(PAGE, page, all), Some(0x11000));
    assert_eq!(find(2 * PAGE, page, all), Some(0x13000));
    assert_eq!(find(PAGE, big, all), Some(0x20000));
    assert_eq!(find(PAGE, PageSize::new(1).unwrap(), range(0x10800, 0x10000)), Some(0x11000), "空間の粒度に揃える");
    assert_eq!(find(2 * PAGE, page, range(0x10000, 3 * PAGE)), None);
    assert_eq!(find(0, page, all), None);
    assert_eq!(find(PAGE, page, range(0xffff_f000, PAGE)), Some(0xffff_f000));
    assert_eq!(find(2 * PAGE, page, range(0xffff_f000, PAGE)), None);
}

#[test]
fn a_reducing_change_bars_the_process_until_old_runs_stop() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (q, _) = process(&mut k);
    let (runner, _) = thread(&mut k, p);
    let (caller, _) = thread(&mut k, p);
    let b = ok!(k, k.create_backing(p, 2 * PAGE));
    ok!(k, k.map(p, range(0x1000, 2 * PAGE), backed(b, 0), Prot::READ_WRITE));
    ok!(k, k.release_backing(p, b));
    let old = ok!(k, k.grant(W0)).unwrap();
    assert_eq!((old.thread(), old.map()), (runner, k.map_generation(p).unwrap()));
    assert_eq!(trap(&mut k, W1), caller);
    // caller の依頼で unmap する。runner は古い世代で実行中である。
    ok!(k, k.unmap(p, range(0x1000, PAGE)));
    ok!(k, k.protect(p, range(0x2000, PAGE), Prot::READ));
    ok!(k, k.unmap(p, range(0x2000, PAGE)));
    assert_eq!(
        drain(&mut k),
        vec![Effect::Interrupt { worker: W0, thread: runner, run: old.run() }],
        "中断は一度だけ要求し、使用中の実体は解放しない"
    );
    // 変更の完了を見るはずの caller は、古い実行が止まるまで再開しない。
    ok!(k, k.continue_thread(caller));
    assert_eq!(ok!(k, k.grant(W1)), None, "古い世代の実行が残るのに実行権を貸した");
    // 別のプロセスは妨げない。
    let (other, _) = thread(&mut k, q);
    let unrelated = ok!(k, k.grant(W1)).unwrap();
    assert_eq!(unrelated.thread(), other);
    let _ = k.relinquish(unrelated, Stop::Trap, Budget::new(0));
    assert_eq!(ok!(k, k.relinquish(old, Stop::Yielded, Budget::new(1))), ThreadFate::Ready);
    assert_eq!(drain(&mut k), vec![Effect::BackingReclaimed { backing: b }]);
    let fresh = ok!(k, k.grant(W0)).unwrap();
    assert_eq!((fresh.thread(), fresh.map()), (caller, k.map_generation(p).unwrap()));
    let _ = k.relinquish(fresh, Stop::Trap, Budget::new(0));
}

#[test]
fn unmapping_a_reservation_interrupts_nobody() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (_runner, _) = thread(&mut k, p);
    ok!(k, k.map(p, range(0x1000, PAGE), MapSource::Reserved, Prot::NONE));
    let permit = ok!(k, k.grant(W0)).unwrap();
    ok!(k, k.unmap(p, range(0x1000, PAGE)));
    assert!(drain(&mut k).is_empty());
    let _ = k.relinquish(permit, Stop::Trap, Budget::new(0));
}

#[test]
fn process_exit_releases_holds_and_mappings() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let b = ok!(k, k.create_backing(p, PAGE));
    ok!(k, k.retain_backing(p, b));
    ok!(k, k.map(p, range(0x1000, PAGE), backed(b, 0), Prot::READ));
    let e = event(&mut k, p);
    ok!(k, k.retain_object(p, e));
    let timer = ok!(k, k.create_timer(p, e));
    ok!(k, k.arm_timer(timer, at(5), None));
    ok!(k, k.exit_process(p, ExitCode::new(0)));
    let effects = drain(&mut k);
    assert!(effects.contains(&Effect::BackingReclaimed { backing: b }));
    assert!(effects.contains(&Effect::ObjectReclaimed { object: e }));
    assert!(is_empty(&k));
}

#[test]
fn limits_are_refusals() {
    let limits = Limits::DEFAULT.with_processes(1).with_threads(1).with_objects(3).with_holdings(1).with_timers(1).with_ios(1);
    let mut k = kernel_with(limits);
    let (p, _) = process(&mut k);
    let page = PageSize::new(PAGE).unwrap();
    assert_eq!(k.create_process(AddrWidth::Bits64, page), Err(Error::Exhausted(Resource::Processes)));
    let (_t, _) = thread(&mut k, p);
    assert_eq!(k.create_thread(p, Start::Ready), Err(Error::Exhausted(Resource::Threads)));
    let e = event(&mut k, p);
    let before = k.snapshot().unwrap();
    assert_eq!(k.create_object(p, SignalSpec::new(SignalMode::Keep, 0, 1)), Err(Error::Exhausted(Resource::Holdings)));
    let _timer = ok!(k, k.create_timer(p, e));
    assert_eq!(k.create_timer(p, e), Err(Error::Exhausted(Resource::Timers)));
    let _io = ok!(k, k.prepare_io(p, IoLinks::NONE));
    assert_eq!(k.prepare_io(p, IoLinks::NONE), Err(Error::Exhausted(Resource::Ios)));
    let _ = before;
    assert_eq!(
        Kernel::new(InstanceId::new(1), Limits::DEFAULT.with_quantum(Budget::new(0))).err(),
        Some(Error::Refused(Refusal::BadLimits))
    );
}

#[test]
fn apply_is_the_same_as_calling_the_method() {
    let mut by_input = kernel();
    let mut by_call = kernel();
    let page = PageSize::new(PAGE).unwrap();
    let Reply::Process(p, _) = by_input.apply(Input::CreateProcess(AddrWidth::Bits32, page)).unwrap() else {
        panic!()
    };
    let (q, _) = by_call.create_process(AddrWidth::Bits32, page).unwrap();
    assert_eq!(p, q);
    let Reply::Thread(t, _) = by_input.apply(Input::CreateThread(p, Start::Ready)).unwrap() else { panic!() };
    by_call.create_thread(q, Start::Ready).unwrap();
    let Reply::Permit(Some(permit)) = by_input.apply(Input::Grant(W0)).unwrap() else { panic!() };
    let other = by_call.grant(W0).unwrap().unwrap();
    assert_eq!(by_input.apply(Input::Relinquish(permit, Stop::Trap, Budget::new(2))), Ok(Reply::Fate(ThreadFate::Servicing)));
    by_call.relinquish(other, Stop::Trap, Budget::new(2)).unwrap();
    let reply = by_input.apply(Input::Wait(t, &[], WaitMode::Any, Deadline::At(at(9)), Interruptible::No));
    assert_eq!(reply, Ok(Reply::Wait(WaitStart::Blocked)));
    by_call.wait(t, &[], WaitMode::Any, Deadline::At(at(9)), Interruptible::No).unwrap();
    let advanced = by_call.advance(at(9)).unwrap();
    assert_eq!((advanced.fired(), advanced.reached(), advanced.more()), (1, at(9), false));
    assert_eq!(by_input.apply(Input::Advance(at(9))), Ok(Reply::Advanced(advanced)));
    assert_eq!(by_input, by_call);
    // Effect の取り出しも入力である。
    let polled = by_call.poll_effect();
    assert!(polled.is_some());
    assert_eq!(by_input.apply(Input::PollEffect), Ok(Reply::Effect(polled)));
    assert_eq!(by_input, by_call);
    // 受理されなかった実行権は、返答に入って戻る。
    ok!(by_input, by_input.continue_thread(t));
    let Reply::Permit(Some(permit)) = by_input.apply(Input::Grant(W0)).unwrap() else { panic!() };
    let Ok(Reply::Returned(permit, reason)) = by_call.apply(Input::Relinquish(permit, Stop::Trap, Budget::new(1))) else {
        panic!("貸していない状態が実行権を受理した")
    };
    assert_eq!(reason, Error::Refused(Refusal::StalePermit));
    assert_eq!(by_input.apply(Input::Relinquish(permit, Stop::Trap, Budget::new(1))), Ok(Reply::Fate(ThreadFate::Servicing)));
}

// ---- 検査が違反を検出できること (台帳の反例試験) ----

/// 生きたスレッド・資源・I/O・マッピング・待機・タイマを一通り持つ状態。
struct World {
    k: Kernel,
    p: ProcessId,
    ready: ThreadId,
    blocked: ThreadId,
    event: ObjectId,
    io: IoId,
    backing: BackingId,
    timer: TimerId,
    permit: RunPermit,
}

fn world() -> World {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (blocked, _) = thread(&mut k, p);
    let (running, _) = thread(&mut k, p);
    let (ready, _) = thread(&mut k, p);
    let event = event(&mut k, p);
    assert_eq!(trap(&mut k, W0), blocked);
    let started = k.wait(blocked, &[event], WaitMode::Any, Deadline::At(at(100)), Interruptible::No);
    assert_eq!(ok!(k, started), WaitStart::Blocked);
    let io = ok!(k, k.prepare_io(p, IoLinks::NONE.with_notify(event)));
    ok!(k, k.issue_io(io));
    let backing = ok!(k, k.create_backing(p, 2 * PAGE));
    ok!(k, k.map(p, range(0x1000, PAGE), backed(backing, 0), Prot::READ));
    ok!(k, k.map(p, range(0x2000, PAGE), backed(backing, PAGE), Prot::READ));
    let timer = ok!(k, k.create_timer(p, event));
    ok!(k, k.arm_timer(timer, at(200), None));
    let permit = ok!(k, k.grant(W0)).unwrap();
    assert_eq!(permit.thread(), running);
    k.audit().unwrap();
    World { k, p, ready, blocked, event, io, backing, timer, permit }
}

fn broken(k: &Kernel) -> Invariant {
    k.audit().unwrap_err().invariant()
}

#[test]
fn audit_detects_two_permits_for_one_thread() {
    let mut w = world();
    let thread = w.permit.thread();
    w.k.running.push(crate::kernel::Lent { worker: W1, thread }).unwrap();
    assert_eq!(broken(&w.k), Invariant::RunOnePermit);
}

#[test]
fn audit_detects_a_runnable_thread_missing_from_the_queue() {
    let mut w = world();
    assert_eq!(w.k.ready.pop_front(), Some(w.ready));
    assert_eq!(broken(&w.k), Invariant::RunQueue);
}

#[test]
fn audit_detects_a_corrupt_free_list() {
    let mut w = world();
    let live = w.k.objects.len();
    w.k.objects.corrupt_live(live + 1);
    assert_eq!(broken(&w.k), Invariant::IdFresh);
    // 取り除いた ID は、もう一度取り除けない。
    let mut w = world();
    assert!(w.k.timers.remove(w.timer).is_some());
    assert!(w.k.timers.remove(w.timer).is_none());
}

#[test]
fn audit_detects_a_lost_wakeup() {
    let mut w = world();
    w.k.objects.get_mut(w.event).unwrap().count = 1;
    assert_eq!(broken(&w.k), Invariant::WaitNoLostWake);
}

#[test]
fn audit_detects_a_wait_missing_from_its_target() {
    let mut w = world();
    w.k.objects.get_mut(w.event).unwrap().waiters.pop();
    assert_eq!(broken(&w.k), Invariant::WaitOnce);
}

#[test]
fn audit_detects_a_wait_resolved_twice() {
    let mut w = world();
    // 確定した待機の登録が対象の待機列に残っている。
    w.k.threads.get_mut(w.blocked).unwrap().run = Run::Servicing;
    assert_eq!(broken(&w.k), Invariant::ObjectRefs);
    let mut w = world();
    let blocked = w.blocked;
    w.k.objects.get_mut(w.event).unwrap().waiters.push(blocked).unwrap();
    assert_eq!(broken(&w.k), Invariant::WaitOnce);
}

#[test]
fn audit_detects_a_freed_backing_still_mapped() {
    let mut w = world();
    w.k.backings.get_mut(w.backing).unwrap().maps = 1;
    assert_eq!(broken(&w.k), Invariant::MemBackingAlive);
    let mut w = world();
    assert!(w.k.backings.remove(w.backing).is_some());
    assert_eq!(broken(&w.k), Invariant::MemBackingAlive);
}

#[test]
fn audit_detects_overlapping_mappings() {
    let mut w = world();
    w.k.processes.get_mut(w.p).unwrap().space.maps.get_mut(0).unwrap().last = 0x2fff;
    assert_eq!(broken(&w.k), Invariant::MemLayout);
}

#[test]
fn audit_detects_limbo_without_old_runs() {
    let mut w = world();
    let backing = w.backing;
    w.k.processes.get_mut(w.p).unwrap().space.limbo.push(backing).unwrap();
    assert_eq!(broken(&w.k), Invariant::MemBarrier);
    let mut w = world();
    w.k.processes.get_mut(w.p).unwrap().stale_runners = 1;
    assert_eq!(broken(&w.k), Invariant::MemBarrier);
}

#[test]
fn audit_detects_a_lost_effect_promise() {
    let mut w = world();
    let promised = w.k.effects.promised();
    w.k.effects.corrupt_promised(promised - 1);
    assert_eq!(broken(&w.k), Invariant::EffectsKept);
}

#[test]
fn audit_detects_a_wrong_reference_count() {
    let mut w = world();
    w.k.objects.get_mut(w.event).unwrap().pins += 1;
    assert_eq!(broken(&w.k), Invariant::ObjectRefs);
    let mut w = world();
    w.k.objects.get_mut(w.event).unwrap().handles = 0;
    assert_eq!(broken(&w.k), Invariant::ObjectRefs);
}

#[test]
fn audit_detects_an_unordered_deadline_list() {
    let mut w = world();
    let first = w.k.due.remove(0).unwrap();
    w.k.due.push(first).unwrap();
    assert_eq!(broken(&w.k), Invariant::TimeOrder);
}

#[test]
fn audit_detects_a_thread_outside_its_process() {
    let mut w = world();
    w.k.processes.get_mut(w.p).unwrap().threads.pop();
    assert_eq!(broken(&w.k), Invariant::ProcessTree);
    let mut w = world();
    w.k.processes.get_mut(w.p).unwrap().ios = 0;
    assert_eq!(broken(&w.k), Invariant::ProcessTree);
}

#[test]
fn audit_detects_an_io_pointing_at_nothing() {
    let mut w = world();
    // 破棄済みの資源を通知先にする (生きている通知先の固定数は合わせておく)。
    let gone = ok!(w.k, w.k.create_object(w.p, SignalSpec::new(SignalMode::Keep, 0, 1)));
    ok!(w.k, w.k.release_object(w.p, gone));
    w.k.ios.get_mut(w.io).unwrap().notify = Some(gone);
    w.k.objects.get_mut(w.event).unwrap().pins -= 1;
    assert_eq!(broken(&w.k), Invariant::IoOnce);
}

#[test]
fn an_internal_fault_is_never_a_guest_refusal() {
    // 失敗の分類は型で分かれている。分類を増やしたら、ここがコンパイルできなくなる。
    fn class(error: Error) -> &'static str {
        match error {
            Error::Stale(_) | Error::Refused(_) => "ゲストへ返せる",
            Error::Exhausted(_) => "再試行できる",
            Error::Foreign => "別インスタンス",
            Error::Broken(_) => "内部の障害",
        }
    }
    let mut w = world();
    // 状態を壊してから操作すると、拒否ではなく内部の障害として報告する。
    w.k.effects.corrupt_promised(0);
    let error = w.k.cancel_io(w.io).unwrap_err();
    assert_eq!(error, Error::Broken(Invariant::EffectsKept));
    assert_eq!(class(error), "内部の障害");
    assert_eq!(class(Error::Refused(Refusal::IoSettled)), "ゲストへ返せる");
}

/// 実際の `Kernel::wait` が下す判定を、数 0..=2 の資源二つについて TLA+ の表として書き出す。
fn tla_wait_table() -> String {
    const NAMES: [&str; 2] = ["a", "b"];
    let orders: [&[usize]; 4] = [&[0], &[1], &[0, 1], &[1, 0]];
    let mut rows = Vec::new();
    for mode in [WaitMode::Any, WaitMode::All] {
        for order in orders {
            for a in 0..=2u64 {
                for b in 0..=2u64 {
                    let mut k = kernel();
                    let (p, _) = process(&mut k);
                    thread(&mut k, p);
                    let t = trap(&mut k, W0);
                    let objects = [semaphore(&mut k, p, a, 2), semaphore(&mut k, p, b, 2)];
                    let targets: Vec<ObjectId> = order.iter().map(|at| objects[*at]).collect();
                    let started = k.wait(t, &targets, mode, Deadline::Never, Interruptible::No);
                    let verdict = match ok!(k, started) {
                        WaitStart::Blocked => "Blocked",
                        WaitStart::Done(WaitOutcome::Signaled { index: 0 }) => "First",
                        WaitStart::Done(WaitOutcome::Signaled { index: 1 }) => "Second",
                        WaitStart::Done(WaitOutcome::AllSignaled) => "All",
                        WaitStart::Done(other) => panic!("期限も中断も無い待機が {other:?} で終わった"),
                    };
                    let after = objects.map(|o| k.signal_count(o).unwrap());
                    let names: Vec<String> = order.iter().map(|at| format!("\"{}\"", NAMES[*at])).collect();
                    rows.push(format!(
                        "    (<<{a}, {b}, <<{}>>, \"{mode:?}\">> :> <<\"{verdict}\", {}, {}>>)",
                        names.join(", "),
                        after[0],
                        after[1]
                    ));
                }
            }
        }
    }
    format!(
        "----------------------------- MODULE WaitTable -----------------------------\n\
         (* crates/kome の Kernel::wait を実際に動かして生成した。手で書き換えない。\n\
         \x20  書き直す: KOME_UPDATE_GOLDEN=1 cargo test -p kome the_tla_wait_table_is_the_kernel *)\n\
         EXTENDS TLC\n\n\
         \\* (a の数, b の数, 対象の列, 条件) から (判定, 消費後の a の数, 消費後の b の数) への表。\n\
         WaitDecide ==\n{}\n\
         =============================================================================\n",
        rows.join(" @@\n")
    )
}

/// TLA+ のモデル (tla/WaitRace.tla) が使う判定は、このカーネルの判定そのものである。
#[test]
fn the_tla_wait_table_is_the_kernel() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tla/WaitTable.tla");
    let table = tla_wait_table();
    if std::env::var("KOME_UPDATE_GOLDEN").is_ok_and(|v| !v.is_empty()) {
        std::fs::write(path, &table).unwrap();
    }
    let on_disk = std::fs::read_to_string(path).unwrap();
    assert_eq!(on_disk, table, "tla/WaitTable.tla がカーネルと違う。KOME_UPDATE_GOLDEN=1 で書き直す");
}

#[test]
fn peeking_shows_the_queue_without_taking_from_it() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    assert_eq!(k.peek_effect(0), None);
    ok!(k, k.exit_process(p, CODE));
    let count = k.pending_effects();
    assert!(count >= 2);
    let seen: Vec<Effect> = (0..count).map(|at| k.peek_effect(at).unwrap()).collect();
    assert!(seen.contains(&Effect::ThreadExited { thread: t, process: p, code: CODE }));
    assert_eq!(k.peek_effect(count), None);
    assert_eq!(k.pending_effects(), count, "見ただけで取り出した");
    assert_eq!(drain(&mut k), seen);
}

#[test]
fn a_snapshot_differs_from_a_state_that_moved_on() {
    let mut k = kernel();
    let before = k.snapshot().unwrap();
    assert!(k == before);
    let _ = process(&mut k);
    assert!(k != before, "変わった状態が写しと等しい");
    assert!(k == k.snapshot().unwrap());
}

#[test]
fn a_second_suspension_of_a_waiting_to_run_thread_only_counts() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    assert_eq!(ok!(k, k.suspend_thread(t)), 0);
    assert_eq!(k.ready_count(), 0);
    assert_eq!(ok!(k, k.suspend_thread(t)), 1);
    assert_eq!(k.suspend_count(t), Ok(2));
    assert_eq!(ok!(k, k.unsuspend_thread(t)), 2);
    assert_eq!(k.ready_count(), 0);
    assert_eq!(ok!(k, k.unsuspend_thread(t)), 1);
    assert_eq!(k.ready_count(), 1);
}

#[test]
fn distinct_io_links_are_pinned_once_each() {
    // 対象と通知先が別の資源なら、それぞれ一つずつ固定する。同じ資源のときだけ二つ固定する。
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let target = event(&mut k, p);
    let notify = event(&mut k, p);
    k.objects.get_mut(target).unwrap().pins = u32::MAX - 1;
    let links = IoLinks::NONE.with_target(target).with_notify(notify);
    assert!(k.prepare_io(p, links).is_ok());
    assert_eq!(k.objects.get(target).unwrap().pins, u32::MAX);
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let both = event(&mut k, p);
    k.objects.get_mut(both).unwrap().pins = u32::MAX - 1;
    let links = IoLinks::NONE.with_target(both).with_notify(both);
    let before = k.snapshot().unwrap();
    assert!(k.prepare_io(p, links).is_err());
    assert!(k == before);
}

// ---- 終了処理の表 (tla/ExitTable.tla) ----

const EXIT_RUNS: [&str; 4] = ["Ready", "Running", "Servicing", "Blocked"];
const EXIT_IOS: [&str; 6] = ["None", "Prepared", "Issued", "CancelRequested", "Settled", "Published"];

fn phase_name(k: &Kernel, thread: ThreadId) -> &'static str {
    match k.thread_phase(thread) {
        Ok(ThreadPhase::Ready) => "Ready",
        Ok(ThreadPhase::Running) => "Running",
        Ok(ThreadPhase::Servicing) => "Servicing",
        Ok(ThreadPhase::Blocked) => "Blocked",
        Err(Error::Stale(_)) => "Gone",
        Err(other) => panic!("スレッドの状態を読めない: {other:?}"),
    }
}

fn life_name(k: &Kernel, process: ProcessId) -> &'static str {
    match k.process_phase(process) {
        Ok(ProcessPhase::Alive) => "Alive",
        Ok(ProcessPhase::Exiting) => "Exiting",
        Ok(ProcessPhase::Draining) => "Draining",
        Err(Error::Stale(_)) => "Reclaimed",
        Err(other) => panic!("プロセスの状態を読めない: {other:?}"),
    }
}

fn io_name(k: &Kernel, io: Option<IoId>) -> &'static str {
    match io.map(|io| k.io_phase(io)) {
        None => "None",
        Some(Ok(IoPhase::Prepared)) => "Prepared",
        Some(Ok(IoPhase::Issued)) => "Issued",
        Some(Ok(IoPhase::CancelRequested)) => "CancelRequested",
        Some(Ok(IoPhase::Settled)) => "Settled",
        Some(Ok(IoPhase::Published)) => "Published",
        Some(Err(Error::Stale(_))) => "Gone",
        Some(Err(other)) => panic!("I/O の状態を読めない: {other:?}"),
    }
}

/// プロセス `p` に、指定した進行状態の I/O を一つ作る。
fn io_in(k: &mut Kernel, p: ProcessId, state: &str) -> Option<IoId> {
    if state == "None" {
        return None;
    }
    let io = ok!(k, k.prepare_io(p, IoLinks::NONE));
    let done = Completion::new(k.instance(), CompletionKind::Io { io, outcome: DONE });
    if state != "Prepared" {
        ok!(k, k.issue_io(io));
    }
    if state == "CancelRequested" {
        assert_eq!(ok!(k, k.cancel_io(io)), CancelReply::Requested);
    }
    if state == "Settled" || state == "Published" {
        ok!(k, k.complete(done));
    }
    if state == "Published" {
        ok!(k, k.publish_io(io));
    }
    assert_eq!(io_name(k, Some(io)), state);
    Some(io)
}

/// スレッド二つのプロセス。一つ目は指定した状態、二つ目は worker 1 で実行中のまま残す。
struct Pair {
    k: Kernel,
    p: ProcessId,
    first: ThreadId,
    /// 一つ目が実行中なら、その実行権。
    permit: Option<RunPermit>,
    /// 二つ目の実行権。
    anchor: RunPermit,
}

fn pair(run: &str) -> Pair {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (first, _) = thread(&mut k, p);
    let _ = thread(&mut k, p);
    let permit = ok!(k, k.grant(W0)).unwrap();
    let anchor = ok!(k, k.grant(W1)).unwrap();
    assert_eq!(permit.thread(), first);
    let permit = match run {
        "Running" => Some(permit),
        "Ready" => {
            assert_eq!(ok!(k, k.relinquish(permit, Stop::Yielded, Budget::new(1))), ThreadFate::Ready);
            None
        }
        "Servicing" | "Blocked" => {
            assert_eq!(ok!(k, k.relinquish(permit, Stop::Trap, Budget::new(1))), ThreadFate::Servicing);
            if run == "Blocked" {
                let gate = event(&mut k, p);
                let started = k.wait(first, &[gate], WaitMode::Any, Deadline::Never, Interruptible::No);
                assert_eq!(ok!(k, started), WaitStart::Blocked);
            }
            None
        }
        other => panic!("知らない状態: {other}"),
    };
    assert_eq!(phase_name(&k, first), run);
    drain(&mut k);
    Pair { k, p, first, permit, anchor }
}

fn tla_exit_table() -> String {
    let code = ExitCode::new(5);
    let interrupts = |effects: &[Effect], thread: ThreadId| {
        effects.iter().any(|effect| matches!(effect, Effect::Interrupt { thread: of, .. } if *of == thread))
    };
    // 終了の要求が、状態ごとのスレッドに何をするか。もう一つのスレッドが実行中なので、
    // プロセスは終了の途中に留まる。
    let mut threads = Vec::new();
    for request in ["ExitProcess", "Terminate"] {
        for run in EXIT_RUNS {
            let mut w = pair(run);
            match request {
                "ExitProcess" => ok!(w.k, w.k.exit_process(w.p, code)),
                _ => ok!(w.k, w.k.terminate_thread(w.first, code)),
            }
            let effects = drain(&mut w.k);
            let after = phase_name(&w.k, w.first);
            let asked = interrupts(&effects, w.first);
            let life = life_name(&w.k, w.p);
            assert_eq!(life, if request == "ExitProcess" { "Exiting" } else { "Alive" });
            threads.push(format!("    (<<\"{request}\", \"{run}\">> :> <<\"{after}\", {}>>)", if asked { "TRUE" } else { "FALSE" }));
            // 終了を求められた実行中のスレッドは、どの理由で実行権を返しても終了する。
            if let Some(permit) = w.permit.take() {
                for stop in [Stop::Yielded, Stop::Trap] {
                    let mut again = pair(run);
                    match request {
                        "ExitProcess" => ok!(again.k, again.k.exit_process(again.p, code)),
                        _ => ok!(again.k, again.k.terminate_thread(again.first, code)),
                    }
                    let held = again.permit.take().unwrap();
                    assert_eq!(ok!(again.k, again.k.relinquish(held, stop, Budget::new(1))), ThreadFate::Exited(code));
                    assert_eq!(phase_name(&again.k, again.first), "Gone");
                    let _ = again.k.relinquish(again.anchor, Stop::Trap, Budget::new(1));
                }
                assert_eq!(ok!(w.k, w.k.relinquish(permit, Stop::Trap, Budget::new(1))), ThreadFate::Exited(code));
            }
            let _ = w.k.relinquish(w.anchor, Stop::Trap, Budget::new(1));
        }
    }
    // 最後のスレッドが止まったときに、I/O の状態ごとにプロセスと I/O がどうなるか。
    // 終了の要求の時点で止まっていた場合と、実行中のスレッドが後から戻った場合で同じになる。
    let mut finishes = Vec::new();
    let mut completes = Vec::new();
    for state in EXIT_IOS {
        let finish = |running: bool| {
            let mut k = kernel();
            let (p, _) = process(&mut k);
            let (t, _) = thread(&mut k, p);
            let held = event(&mut k, p);
            let permit = ok!(k, k.grant(W0)).unwrap();
            assert_eq!(permit.thread(), t);
            let io = io_in(&mut k, p, state);
            drain(&mut k);
            let permit = if running {
                Some(permit)
            } else {
                assert_eq!(ok!(k, k.relinquish(permit, Stop::Trap, Budget::new(1))), ThreadFate::Servicing);
                None
            };
            ok!(k, k.exit_process(p, code));
            if let Some(permit) = permit {
                // 実行中のスレッドが残る間は、何も手放さない。
                assert_eq!((life_name(&k, p), io_name(&k, io)), ("Exiting", state));
                assert!(k.signal_count(held).is_ok());
                assert_eq!(k.exit_process(p, code), Err(Error::Refused(Refusal::ProcessExiting)));
                assert_eq!(ok!(k, k.relinquish(permit, Stop::Yielded, Budget::new(1))), ThreadFate::Exited(code));
            }
            let effects = drain(&mut k);
            let cancels = effects.iter().any(|effect| matches!(effect, Effect::IoCancel { .. }));
            let exited = effects.iter().filter(|effect| matches!(effect, Effect::ProcessExited { .. })).count();
            // 終了の通知は一度だけで、プロセスが持っていた参照はその時点で全て手放している。
            assert_eq!(exited, 1);
            assert_eq!(k.signal_count(held), Err(Error::Stale(Tag::Object)));
            (k, p, io, cancels)
        };
        let (mut k, p, io, cancels) = finish(false);
        let (later, later_p, later_io, later_cancels) = finish(true);
        let row = (life_name(&k, p), io_name(&k, io), cancels);
        assert_eq!(row, (life_name(&later, later_p), io_name(&later, later_io), later_cancels), "{state}");
        let cancels = if row.2 { "TRUE" } else { "FALSE" };
        finishes.push(format!("    (\"{state}\" :> <<\"{}\", \"{}\", {cancels}>>)", row.0, row.1));
        // 終了の通知の後に届いた完了通知。
        if let (Some(io), "Draining") = (io, row.0) {
            let waiting = row.1;
            let done = Completion::new(k.instance(), CompletionKind::Io { io, outcome: DONE });
            assert_eq!(k.exit_process(p, code), Err(Error::Refused(Refusal::ProcessExiting)));
            ok!(k, k.complete(done));
            let effects = drain(&mut k);
            assert_eq!(effects, vec![Effect::ProcessReclaimed { process: p }]);
            completes.push(format!("    (\"{waiting}\" :> <<\"{}\", \"{}\">>)", life_name(&k, p), io_name(&k, Some(io))));
        }
    }
    completes.sort();
    completes.dedup();
    format!(
        "----------------------------- MODULE ExitTable -----------------------------\n\
         (* crates/kome の Kernel を実際に動かして生成した。手で書き換えない。\n\
         \x20  書き直す: KOME_UPDATE_GOLDEN=1 cargo test -p kome the_tla_exit_table_is_the_kernel *)\n\
         EXTENDS TLC\n\n\
         \\* (終了の要求, スレッドの状態) から (要求の直後の状態, 中断を要求したか) への表。\n\
         \\* 実行中のスレッドは、実行権を返したときに終了する。\n\
         ExitThread ==\n{}\n\n\
         \\* 最後のスレッドが止まったとき: I/O の状態から (プロセスの段階, I/O の状態, 取消を要求したか) への表。\n\
         \\* プロセスが持っていた参照は、このとき全て手放す。終了の通知はこのとき一度だけ出す。\n\
         ExitFinish ==\n{}\n\n\
         \\* 終了の通知の後に完了通知が届いたとき: I/O の状態から (プロセスの段階, I/O の状態) への表。\n\
         ExitComplete ==\n{}\n\
         =============================================================================\n",
        threads.join(" @@\n"),
        finishes.join(" @@\n"),
        completes.join(" @@\n")
    )
}

/// TLA+ のモデル (tla/ProcessExit.tla) が使う表は、このカーネルの動きそのものである。
#[test]
fn the_tla_exit_table_is_the_kernel() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tla/ExitTable.tla");
    let table = tla_exit_table();
    if std::env::var("KOME_UPDATE_GOLDEN").is_ok_and(|v| !v.is_empty()) {
        std::fs::write(path, &table).unwrap();
    }
    let on_disk = std::fs::read_to_string(path).unwrap_or_default();
    assert_eq!(on_disk, table, "tla/ExitTable.tla がカーネルと違う。KOME_UPDATE_GOLDEN=1 で書き直す");
}

// ---- 検査の文言ごとの壊し方 ----

fn thread_mut(k: &mut Kernel, thread: ThreadId) -> &mut crate::thread::Thread {
    k.threads.get_mut(thread).unwrap()
}

fn process_mut(k: &mut Kernel, process: ProcessId) -> &mut crate::process::Process {
    k.processes.get_mut(process).unwrap()
}

fn wait_mut(k: &mut Kernel, thread: ThreadId) -> &mut crate::wait::Wait {
    match &mut thread_mut(k, thread).run {
        Run::Blocked(wait) => wait,
        other @ (Run::Ready | Run::Running { .. } | Run::Servicing) => panic!("待機中ではない: {other:?}"),
    }
}

fn mapping_mut(k: &mut Kernel, process: ProcessId, at: usize) -> &mut crate::mem::Mapping {
    process_mut(k, process).space.maps.get_mut(at).unwrap()
}

/// `world` を一か所だけ壊した状態。
fn bent(change: impl FnOnce(&mut World)) -> Kernel {
    let mut w = world();
    change(&mut w);
    w.k
}

/// I/O の応答待ちだけが残った終了済みのプロセス `p` と、生きているプロセス `q`。
struct Drained {
    k: Kernel,
    p: ProcessId,
    io: IoId,
    q: ProcessId,
    thread: ThreadId,
    object: ObjectId,
    backing: BackingId,
    timer: TimerId,
}

fn drained(change: impl FnOnce(&mut Drained)) -> Kernel {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let io = ok!(k, k.prepare_io(p, IoLinks::NONE));
    ok!(k, k.issue_io(io));
    ok!(k, k.exit_process(p, ExitCode::new(3)));
    assert_eq!(k.process_phase(p), Ok(ProcessPhase::Draining));
    let (q, _) = process(&mut k);
    let (thread, _) = thread(&mut k, q);
    let object = event(&mut k, q);
    let backing = ok!(k, k.create_backing(q, PAGE));
    let timer = ok!(k, k.create_timer(q, object));
    let mut d = Drained { k, p, io, q, thread, object, backing, timer };
    d.k.audit().unwrap();
    change(&mut d);
    d.k
}

const CODE: ExitCode = ExitCode::new(1);

/// 検査の文言ごとに、その違反だけを起こした状態を作り、検査がその文言で報告することを確かめる。
/// 同じ文言に、条件の一部分ずつを破る状態を並べる。
#[test]
fn audit_names_every_violation_it_can_report() {
    let cases: Vec<(&'static str, Kernel)> = {
    let exiting_with_a_runner_nobody_asked_to_stop = {
            let mut k = kernel();
            let (p, _) = process(&mut k);
            thread(&mut k, p);
            let _lent = ok!(k, k.grant(W0)).unwrap();
            process_mut(&mut k, p).life = crate::process::Life::Exiting(CODE);
            k
        };
        let exiting_without_threads = {
            let mut k = kernel();
            let (p, _) = process(&mut k);
            process_mut(&mut k, p).life = crate::process::Life::Exiting(CODE);
            k
        };
        vec![
            ("表の空きスロットと生存数が一致しない", bent(|w| w.k.objects.corrupt_live(w.k.objects.len() + 1))),
            (
                "貸し出しが worker の上限を超えている",
                bent(|w| {
                    w.k.running.push(crate::kernel::Lent { worker: W1, thread: w.ready }).unwrap();
                    w.k.running.push(crate::kernel::Lent { worker: WorkerId::new(2), thread: w.blocked }).unwrap();
                }),
            ),
            (
                "一つの worker が二つの実行権を持っている",
                bent(|w| w.k.running.push(crate::kernel::Lent { worker: W0, thread: w.ready }).unwrap()),
            ),
            (
                "一つのスレッドに二つの実行権がある",
                bent(|w| w.k.running.push(crate::kernel::Lent { worker: W1, thread: w.permit.thread() }).unwrap()),
            ),
            (
                "貸し出しの記録とスレッドの状態が一致しない",
                bent(|w| w.k.running.push(crate::kernel::Lent { worker: W1, thread: w.ready }).unwrap()),
            ),
            ("実行中のスレッドに貸し出しの記録が無い", bent(|w| assert!(w.k.running.pop().is_some()))),
            (
                "実行中のスレッドが実行待ちの列にある",
                bent(|w| {
                    w.k.running.push(crate::kernel::Lent { worker: W1, thread: w.ready }).unwrap();
                    thread_mut(&mut w.k, w.ready).run = Run::Running { worker: W1, stale: false, interrupted: false };
                }),
            ),
            ("実行可能なスレッドと実行待ちの列が一致しない", bent(|w| assert!(w.k.ready.pop_front().is_some()))),
            ("終了要求が実行待ちのスレッドに残っている", bent(|w| thread_mut(&mut w.k, w.ready).kill = Some(CODE))),
            ("実行できないスレッドが実行待ちの列にある", bent(|w| thread_mut(&mut w.k, w.ready).run = Run::Servicing)),
            ("終了要求が止まっているスレッドに残っている", bent(|w| thread_mut(&mut w.k, w.blocked).kill = Some(CODE))),
            ("実行待ちの列に、終了したスレッドがある", bent(|w| assert!(w.k.threads.remove(w.ready).is_some()))),
            (
                "スレッドが所属プロセスの一覧にちょうど一度現れない",
                bent(|w| assert!(process_mut(&mut w.k, w.p).threads.pop().is_some())),
            ),
            (
                "プロセスの一覧に、所属していないスレッドがある",
                bent(|w| {
                    let (q, _) = process(&mut w.k);
                    let (other, _) = thread(&mut w.k, q);
                    process_mut(&mut w.k, w.p).threads.push(other).unwrap();
                }),
            ),
            ("プロセスの参照の記録が整列していない", bent(|w| process_mut(&mut w.k, w.p).objects.corrupt_first_count(0))),
            ("プロセスの参照の記録が整列していない", bent(|w| process_mut(&mut w.k, w.p).backings.corrupt_first_count(0))),
            ("プロセスの I/O 数が一致しない", bent(|w| process_mut(&mut w.k, w.p).ios = 0)),
            ("古い世代の実行の数が一致しない", bent(|w| process_mut(&mut w.k, w.p).stale_runners = 1)),
            (
                "古い世代の実行が無いのに保留が残っている",
                bent(|w| process_mut(&mut w.k, w.p).space.limbo.push(w.backing).unwrap()),
            ),
            (
                "終了中のプロセスに、終了要求の無いスレッドがある",
                bent(|w| process_mut(&mut w.k, w.p).life = crate::process::Life::Exiting(CODE)),
            ),
            ("終了中のプロセスに、終了要求の無いスレッドがある", exiting_with_a_runner_nobody_asked_to_stop),
            ("終了中のプロセスに、終了要求の無いスレッドがある", exiting_without_threads),
            (
                "終了済みのプロセスが資源を持っている",
                drained(|d| {
                    process_mut(&mut d.k, d.q).threads.pop();
                    process_mut(&mut d.k, d.p).threads.push(d.thread).unwrap();
                    thread_mut(&mut d.k, d.thread).process = d.p;
                }),
            ),
            (
                "終了済みのプロセスが資源を持っている",
                drained(|d| process_mut(&mut d.k, d.p).objects.add(d.object, 100).unwrap()),
            ),
            (
                "終了済みのプロセスが資源を持っている",
                drained(|d| process_mut(&mut d.k, d.p).backings.add(d.backing, 100).unwrap()),
            ),
            (
                "終了済みのプロセスが資源を持っている",
                drained(|d| {
                    let reserved =
                        crate::mem::Mapping { start: PAGE, last: 2 * PAGE - 1, prot: Prot::NONE, source: MapSource::Reserved };
                    process_mut(&mut d.k, d.p).space.maps.push(reserved).unwrap();
                }),
            ),
            (
                "終了済みのプロセスに、取消を要求していない I/O がある",
                drained(|d| d.k.ios.get_mut(d.io).unwrap().state = crate::io::IoState::Issued),
            ),
            (
                "終了済みのプロセスに、取消を要求していない I/O がある",
                drained(|d| {
                    assert!(d.k.ios.remove(d.io).is_some());
                    process_mut(&mut d.k, d.p).ios = 0;
                }),
            ),
            ("終了済みのプロセスがタイマを持っている", drained(|d| d.k.timers.get_mut(d.timer).unwrap().process = d.p)),
            (
                "I/O が、存在しないプロセスまたは資源を指している",
                bent(|w| {
                    let gone = ok!(w.k, w.k.create_object(w.p, SignalSpec::new(SignalMode::Keep, 0, 1)));
                    ok!(w.k, w.k.release_object(w.p, gone));
                    w.k.ios.get_mut(w.io).unwrap().notify = Some(gone);
                    w.k.objects.get_mut(w.event).unwrap().pins -= 1;
                }),
            ),
            (
                "I/O が、存在しないプロセスまたは資源を指している",
                bent(|w| {
                    let (gone, _) = process(&mut w.k);
                    ok!(w.k, w.k.exit_process(gone, CODE));
                    w.k.ios.get_mut(w.io).unwrap().process = gone;
                    process_mut(&mut w.k, w.p).ios = 0;
                }),
            ),
            ("資源の保持数が一致しない", bent(|w| w.k.objects.get_mut(w.event).unwrap().handles = 0)),
            ("資源の固定数が一致しない", bent(|w| w.k.objects.get_mut(w.event).unwrap().pins += 1)),
            (
                "参照の無い資源が残っている",
                bent(|w| {
                    let lone = event(&mut w.k, w.p);
                    process_mut(&mut w.k, w.p).objects.sub(lone).unwrap();
                    w.k.objects.get_mut(lone).unwrap().handles = 0;
                }),
            ),
            ("資源の数が上限を超えている", bent(|w| w.k.objects.get_mut(w.event).unwrap().count = 2)),
            (
                "待機が対象の待機列にちょうど一度現れない",
                bent(|w| assert!(w.k.objects.get_mut(w.event).unwrap().waiters.pop().is_some())),
            ),
            ("待機の対象が重複している", bent(|w| wait_mut(&mut w.k, w.blocked).targets.push(w.event).unwrap())),
            ("成立している待機が登録されたまま残っている", bent(|w| w.k.objects.get_mut(w.event).unwrap().count = 1)),
            ("待機の期限が期限の列にちょうど一度現れない", bent(|w| w.k.due.get_mut(0).unwrap().seq = 999)),
            ("期限を過ぎた待機が登録されたまま残っている", bent(|w| w.k.now = at(100))),
            (
                "待機列に、この資源を待っていないスレッドがある",
                bent(|w| w.k.objects.get_mut(w.event).unwrap().waiters.push(w.ready).unwrap()),
            ),
            (
                "期限の列が (時刻, 登録順) の順でない",
                bent(|w| {
                    let first = w.k.due.remove(0).unwrap();
                    w.k.due.push(first).unwrap();
                }),
            ),
            ("期限の列に、対応する待機もタイマも無い項目がある", bent(|w| w.k.due.get_mut(1).unwrap().seq += 1)),
            (
                "期限の列の長さが、設定済みのタイマと期限付きの待機の数に一致しない",
                bent(|w| assert!(w.k.due.pop().is_some())),
            ),
            (
                "タイマが、存在しないプロセスまたは資源を指している",
                bent(|w| {
                    let gone = event(&mut w.k, w.p);
                    ok!(w.k, w.k.release_object(w.p, gone));
                    w.k.timers.get_mut(w.timer).unwrap().object = gone;
                    w.k.objects.get_mut(w.event).unwrap().pins -= 1;
                }),
            ),
            (
                "タイマが、存在しないプロセスまたは資源を指している",
                bent(|w| {
                    let (gone, _) = process(&mut w.k);
                    ok!(w.k, w.k.exit_process(gone, CODE));
                    w.k.timers.get_mut(w.timer).unwrap().process = gone;
                }),
            ),
            ("マッピングが昇順でない、または重なっている", bent(|w| mapping_mut(&mut w.k, w.p, 1).start = 2 * PAGE - 1)),
            ("マッピングが粒度に揃っていない", bent(|w| mapping_mut(&mut w.k, w.p, 1).last = 3 * PAGE - 2)),
            ("マッピングが粒度に揃っていない", bent(|w| mapping_mut(&mut w.k, w.p, 0).start = PAGE + 1)),
            ("マッピングが幅を超えている", bent(|w| mapping_mut(&mut w.k, w.p, 1).last = 0x1_0000_0fff)),
            (
                "予約だけのマッピングが権限を持っている",
                bent(|w| mapping_mut(&mut w.k, w.p, 0).source = MapSource::Reserved),
            ),
            (
                "マッピングが、解放済みの実体または実体の外を指している",
                bent(|w| w.k.backings.get_mut(w.backing).unwrap().len = 2 * PAGE - 1),
            ),
            (
                "マッピングが、解放済みの実体または実体の外を指している",
                bent(|w| assert!(w.k.backings.remove(w.backing).is_some())),
            ),
            ("実体の中の位置が揃っていない", bent(|w| mapping_mut(&mut w.k, w.p, 0).source = backed(w.backing, 1))),
            ("マッピングの数が上限を超えている", bent(|w| w.k.limits = w.k.limits.with_mappings(1))),
            (
                "保留が、解放済みの実体を指している",
                bent(|w| {
                    let gone = ok!(w.k, w.k.create_backing(w.p, PAGE));
                    ok!(w.k, w.k.release_backing(w.p, gone));
                    let running = w.permit.thread();
                    thread_mut(&mut w.k, running).run = Run::Running { worker: W0, stale: true, interrupted: false };
                    process_mut(&mut w.k, w.p).stale_runners = 1;
                    process_mut(&mut w.k, w.p).space.limbo.push(gone).unwrap();
                }),
            ),
            (
                "保留に、同じ実体が二度入っている",
                bent(|w| {
                    let running = w.permit.thread();
                    thread_mut(&mut w.k, running).run = Run::Running { worker: W0, stale: true, interrupted: false };
                    process_mut(&mut w.k, w.p).stale_runners = 1;
                    process_mut(&mut w.k, w.p).space.limbo.push(w.backing).unwrap();
                    process_mut(&mut w.k, w.p).space.limbo.push(w.backing).unwrap();
                }),
            ),
            ("実体の参照の数が一致しない", bent(|w| w.k.backings.get_mut(w.backing).unwrap().maps = 1)),
            ("実体の参照の数が一致しない", bent(|w| w.k.backings.get_mut(w.backing).unwrap().holds = 2)),
            ("実体の参照の数が一致しない", bent(|w| w.k.backings.get_mut(w.backing).unwrap().limbo = 1)),
            (
                "参照の無い実体が残っている",
                bent(|w| {
                    let lone = ok!(w.k, w.k.create_backing(w.p, PAGE));
                    process_mut(&mut w.k, w.p).backings.sub(lone).unwrap();
                    w.k.backings.get_mut(lone).unwrap().holds = 0;
                }),
            ),
            ("実体の合計バイト数が一致しない", bent(|w| w.k.backing_bytes += 1)),
            ("実体の合計バイト数が一致しない", bent(|w| w.k.limits = w.k.limits.with_backing_bytes(PAGE))),
            (
                "Effect の予約数が、状態から数えた数と一致しない",
                bent(|w| w.k.effects.corrupt_promised(w.k.effects.promised() - 1)),
            ),
            ("Effect のキューと予約が上限を超えている", bent(|w| w.k.effects.corrupt_limit(0))),
            ("表が、取り除いた番号を戻すための容量を持っていない", bent(|w| w.k.timers.corrupt_capacity())),
            ("表が、取り除いた番号を戻すための容量を持っていない", bent(|w| w.k.processes.corrupt_capacity())),
            ("実行権の表が、worker の上限ぶんの容量を持っていない", bent(|w| w.k.running.corrupt_capacity())),
            ("実行待ちの列が、スレッドの数ぶんの容量を持っていない", bent(|w| w.k.ready.corrupt_capacity())),
            ("Effect のキューが、上限ぶんの容量を持っていない", bent(|w| w.k.effects.corrupt_capacity())),
            ("通信路の端の保持数が一致しない", piped(|k, _, c| k.channels.get_mut(c).unwrap().corrupt_senders(2))),
            (
                "端の保持が無い通信路が残っている",
                piped(|k, p, c| {
                    process_mut(k, p).senders.sub(c).unwrap();
                    process_mut(k, p).receivers.sub(c).unwrap();
                    let channel = k.channels.get_mut(c).unwrap();
                    channel.corrupt_senders(0);
                    channel.receivers = 0;
                }),
            ),
            (
                "通信路の中身が容量を超えている",
                piped(|k, _, c| {
                    let channel = k.channels.get_mut(c).unwrap();
                    channel.capacity = 1;
                    k.channel_bytes = 1;
                }),
            ),
            (
                "通信路のメッセージの長さの合計が中身と一致しない",
                piped(|k, _, c| assert_eq!(k.channels.get_mut(c).unwrap().bytes.drain_into(&mut [0u8; 1]), 1)),
            ),
            (
                "受ける端の無い通信路に中身が残っている",
                piped(|k, p, c| {
                    process_mut(k, p).receivers.sub(c).unwrap();
                    k.channels.get_mut(c).unwrap().receivers = 0;
                }),
            ),
            (
                "通信路の通知の数が中身と一致しない",
                piped(|k, _, c| {
                    let readable = k.channels.get(c).unwrap().readable;
                    k.objects.get_mut(readable).unwrap().count = 0;
                }),
            ),
            (
                "通信路の通知の数が中身と一致しない",
                piped(|k, _, c| {
                    let writable = k.channels.get(c).unwrap().writable;
                    k.objects.get_mut(writable).unwrap().driven = false;
                }),
            ),
            ("通信路が持てるバイト数の合計が一致しない", piped(|k, _, _| k.channel_bytes += 1)),
            ("通信路が持てるバイト数の合計が一致しない", piped(|k, _, _| k.limits = k.limits.with_channel_bytes(1))),
            ("通信路が、容量ぶんの記憶域を持っていない", piped(|k, _, c| k.channels.get_mut(c).unwrap().frames.corrupt_capacity())),
            ("資源の固定数が一致しない", piped(|k, _, c| k.channels.get_mut(c).unwrap().frames.clear())),
            (
                "実体の参照の数が一致しない",
                piped(|k, p, _| {
                    let backing = ok!(k, k.create_backing(p, PAGE));
                    k.backings.get_mut(backing).unwrap().transit = 1;
                }),
            ),
        ]
    };
    for (at, (detail, k)) in cases.iter().enumerate() {
        assert_eq!(k.audit().map_err(|v| v.detail()), Err(*detail), "壊し方 {at}");
    }
    // audit.rs が報告しうる文言は、どれも上の壊し方を持つ。文言を足したら壊し方も足す。
    let stated: alloc::collections::BTreeSet<&str> =
        include_str!("audit.rs").split('"').skip(1).step_by(2).filter(|text| !text.is_ascii()).collect();
    let covered: alloc::collections::BTreeSet<&str> = cases.iter().map(|(detail, _)| *detail).collect();
    assert_eq!(stated, covered);
}

// ---- 通信路 ----

/// 一つのプロセスが両端を持つ、メッセージ 2 個 (各 8 バイトまで) の通信路。
fn pipe(k: &mut Kernel, p: ProcessId) -> ChannelCreated {
    ok!(k, k.create_channel(p, ChannelSpec::messages(2, 8)))
}

/// 資源を運ぶメッセージが一つ入った通信路を持つ状態を作り、`change` で壊す。
fn piped(change: impl FnOnce(&mut Kernel, ProcessId, ChannelId)) -> Kernel {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let made = pipe(&mut k, p);
    let carried = event(&mut k, p);
    assert_eq!(ok!(k, k.send(p, made.channel(), b"abc", Some(Parcel::Object(carried)))), Sent::Accepted(3));
    change(&mut k, p, made.channel());
    k
}

fn take(k: &mut Kernel, p: ProcessId, c: ChannelId, room: usize) -> Result<(Vec<u8>, Option<Parcel>), Received> {
    let mut buffer = vec![0xEEu8; room];
    match ok!(k, k.receive(p, c, &mut buffer)) {
        Received::Data { bytes, parcel } => {
            let bytes = usize::try_from(bytes).unwrap();
            assert!(buffer[bytes..].iter().all(|byte| *byte == 0xEE), "受け取った長さの外へ書いた");
            buffer.truncate(bytes);
            Ok((buffer, parcel))
        }
        other @ (Received::Empty | Received::Ended) => Err(other),
    }
}

#[test]
fn audit_detects_a_channel_out_of_step() {
    assert_eq!(broken(&piped(|k, _, c| k.channels.get_mut(c).unwrap().corrupt_senders(2))), Invariant::ChannelKept);
    assert_eq!(broken(&piped(|k, _, _| k.channel_bytes += 1)), Invariant::ChannelKept);
}

#[test]
fn messages_keep_their_boundaries_and_order() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let made = pipe(&mut k, p);
    let c = made.channel();
    assert_eq!((k.signal_count(made.readable()), k.signal_count(made.writable())), (Ok(0), Ok(1)));
    assert_eq!(take(&mut k, p, c, 8), Err(Received::Empty));
    assert_eq!(k.send(p, c, b"123456789", None), Err(Error::Refused(Refusal::MessageTooLong)));
    assert_eq!(ok!(k, k.send(p, c, b"ab", None)), Sent::Accepted(2));
    assert_eq!((k.signal_count(made.readable()), k.signal_count(made.writable())), (Ok(1), Ok(1)));
    // 長さ 0 のメッセージも一つのメッセージである。
    assert_eq!(ok!(k, k.send(p, c, b"", None)), Sent::Accepted(0));
    assert_eq!(k.signal_count(made.writable()), Ok(0));
    let before = k.snapshot().unwrap();
    assert_eq!(ok!(k, k.send(p, c, b"cd", None)), Sent::Full);
    assert_eq!(k.receive(p, c, &mut [0u8; 1]), Err(Error::Refused(Refusal::BufferTooSmall)));
    assert_eq!(k, before, "受け付けなかった送信と受信が状態を変えた");
    assert_eq!((k.channel_queued(c), k.next_message_len(c)), (Ok(2), Ok(Some(2))));
    assert_eq!(take(&mut k, p, c, 8), Ok((b"ab".to_vec(), None)));
    assert_eq!(k.next_message_len(c), Ok(Some(0)));
    assert_eq!(k.signal_count(made.writable()), Ok(1));
    assert_eq!(take(&mut k, p, c, 0), Ok((Vec::new(), None)));
    assert_eq!((k.signal_count(made.readable()), k.next_message_len(c)), (Ok(0), Ok(None)));
    // 満杯まで入れて取り出すことを繰り返しても、順序と内容は保たれる (入れ物は輪になっている)。
    for round in 0u8..20 {
        assert_eq!(ok!(k, k.send(p, c, &[round; 8], None)), Sent::Accepted(8));
        assert_eq!(ok!(k, k.send(p, c, &[round, 1, 2], None)), Sent::Accepted(3));
        assert_eq!(take(&mut k, p, c, 8), Ok((vec![round; 8], None)));
        assert_eq!(take(&mut k, p, c, 8), Ok((vec![round, 1, 2], None)));
    }
}

#[test]
fn a_stream_takes_what_fits_and_has_no_boundaries() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let made = ok!(k, k.create_channel(p, ChannelSpec::stream(4)));
    let c = made.channel();
    assert_eq!(ok!(k, k.send(p, c, b"abcdef", None)), Sent::Accepted(4));
    assert_eq!(k.signal_count(made.writable()), Ok(0));
    assert_eq!(ok!(k, k.send(p, c, b"ef", None)), Sent::Full);
    assert_eq!(take(&mut k, p, c, 3), Ok((b"abc".to_vec(), None)));
    assert_eq!(ok!(k, k.send(p, c, b"efgh", None)), Sent::Accepted(3));
    assert_eq!((k.channel_queued(c), k.next_message_len(c)), (Ok(4), Ok(Some(4))));
    assert_eq!(take(&mut k, p, c, 8), Ok((b"defg".to_vec(), None)));
    assert_eq!(take(&mut k, p, c, 8), Err(Received::Empty));
    assert_eq!(ok!(k, k.send(p, c, b"", None)), Sent::Accepted(0));
    let o = event(&mut k, p);
    assert_eq!(k.send(p, c, b"x", Some(Parcel::Object(o))), Err(Error::Refused(Refusal::ParcelOnStream)));
}

#[test]
fn readers_and_writers_wait_on_the_channel() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (reader, _) = thread(&mut k, p);
    let (writer, _) = thread(&mut k, p);
    let made = ok!(k, k.create_channel(p, ChannelSpec::stream(2)));
    let c = made.channel();
    assert_eq!(trap(&mut k, W0), reader);
    assert_eq!(wait_any(&mut k, reader, &[made.readable()]), WaitStart::Blocked);
    assert_eq!(ok!(k, k.send(p, c, b"hi", None)), Sent::Accepted(2));
    assert_eq!(drain(&mut k), vec![Effect::WaitDone { thread: reader, outcome: WaitOutcome::Signaled { index: 0 } }]);
    // 読める間は、何度待ってもすぐ成立する (数は中身が決める)。
    assert_eq!(wait_any(&mut k, reader, &[made.readable()]), WaitStart::Done(WaitOutcome::Signaled { index: 0 }));
    assert_eq!(trap(&mut k, W0), writer);
    assert_eq!(wait_any(&mut k, writer, &[made.writable()]), WaitStart::Blocked);
    assert_eq!(take(&mut k, p, c, 1), Ok((b"h".to_vec(), None)));
    assert_eq!(drain(&mut k), vec![Effect::WaitDone { thread: writer, outcome: WaitOutcome::Signaled { index: 0 } }]);
    // 通知の数は外からは変えられない。
    assert_eq!(k.signal(made.readable(), SignalOp::Set(0)), Err(Error::Refused(Refusal::DrivenObject)));
    assert_eq!(k.signal(made.writable(), SignalOp::Add(1)), Err(Error::Refused(Refusal::DrivenObject)));
    // タイマや I/O の通知先にもできない (kome の外の出来事で数が変わってしまう)。
    let driven = Err(Error::Refused(Refusal::DrivenObject));
    assert_eq!(k.create_timer(p, made.readable()).map(|_| ()), driven);
    assert_eq!(k.prepare_io(p, IoLinks::NONE.with_notify(made.writable())).map(|_| ()), driven);
    let both = IoLinks::NONE.with_target(made.readable()).with_notify(made.readable());
    assert_eq!(k.prepare_io(p, both).map(|_| ()), driven);
    // I/O の対象として生かしておくことはできる。
    let io = ok!(k, k.prepare_io(p, IoLinks::NONE.with_target(made.readable())));
    ok!(k, k.release_io(io));
}

#[test]
fn a_closed_end_is_seen_by_the_other_side() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (q, _) = process(&mut k);
    let made = pipe(&mut k, p);
    let c = made.channel();
    ok!(k, k.retain_channel(q, c, End::Receive));
    ok!(k, k.release_channel(p, c, End::Receive));
    assert_eq!(k.receive(p, c, &mut [0u8; 8]), Err(Error::Refused(Refusal::NotHeld)));
    assert_eq!(k.send(q, c, b"x", None), Err(Error::Refused(Refusal::NotHeld)));
    assert_eq!(ok!(k, k.send(p, c, b"last", None)), Sent::Accepted(4));
    // 送る端が無くなっても、残りは読める。読み切ると終わりが分かる。
    ok!(k, k.release_channel(p, c, End::Send));
    assert_eq!(k.release_channel(p, c, End::Send), Err(Error::Refused(Refusal::NotHeld)));
    assert_eq!(k.retain_channel(p, c, End::Send), Err(Error::Refused(Refusal::EndClosed)));
    assert_eq!(take(&mut k, q, c, 8), Ok((b"last".to_vec(), None)));
    assert_eq!(take(&mut k, q, c, 8), Err(Received::Ended));
    assert_eq!(k.signal_count(made.readable()), Ok(1), "終わりは「読める」として通知する");
    // 最後の端が無くなると回収し、通知の資源も破棄する。
    assert_eq!(k.census().channels(), 1);
    ok!(k, k.release_channel(q, c, End::Receive));
    assert_eq!(k.census().channels(), 0);
    assert_eq!(k.channel_queued(c), Err(Error::Stale(Tag::Channel)));
    assert_eq!(
        drain(&mut k),
        vec![Effect::ObjectReclaimed { object: made.readable() }, Effect::ObjectReclaimed { object: made.writable() }]
    );
}

#[test]
fn a_channel_without_receivers_drops_its_content() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let made = pipe(&mut k, p);
    let c = made.channel();
    let carried = event(&mut k, p);
    assert_eq!(ok!(k, k.send(p, c, b"abc", Some(Parcel::Object(carried)))), Sent::Accepted(3));
    assert_eq!(ok!(k, k.send(p, c, b"de", None)), Sent::Accepted(2));
    assert_eq!(k.signal_count(made.writable()), Ok(0));
    ok!(k, k.release_channel(p, c, End::Receive));
    // 運んでいた保持は誰にも渡らずに手放され、資源は破棄される。
    assert_eq!(drain(&mut k), vec![Effect::ObjectReclaimed { object: carried }]);
    assert_eq!((k.channel_queued(c), k.signal_count(made.writable())), (Ok(0), Ok(1)));
    assert_eq!(ok!(k, k.send(p, c, b"x", None)), Sent::NoReceiver);
    assert_eq!(k.channel_queued(c), Ok(0));
    assert_eq!(k.retain_channel(p, c, End::Receive), Err(Error::Refused(Refusal::EndClosed)));
}

#[test]
fn a_message_carries_a_hold_from_sender_to_receiver() {
    let mut k = kernel_with(Limits::DEFAULT.with_holdings(2));
    let (p, _) = process(&mut k);
    let (q, _) = process(&mut k);
    let c = pipe(&mut k, p).channel();
    ok!(k, k.retain_channel(q, c, End::Receive));
    let o = event(&mut k, p);
    let b = ok!(k, k.create_backing(p, PAGE));
    // 持っていない資源は送れない。
    assert_eq!(k.send(q, c, b"", Some(Parcel::Object(o))), Err(Error::Refused(Refusal::NotHeld)));
    ok!(k, k.retain_channel(q, c, End::Send));
    assert_eq!(k.send(q, c, b"", Some(Parcel::Object(o))), Err(Error::Refused(Refusal::NotHeld)));
    assert_eq!(k.send(q, c, b"", Some(Parcel::Backing(b))), Err(Error::Refused(Refusal::NotHeld)));
    assert_eq!(ok!(k, k.send(p, c, b"obj", Some(Parcel::Object(o)))), Sent::Accepted(3));
    assert_eq!(ok!(k, k.send(p, c, b"mem", Some(Parcel::Backing(b)))), Sent::Accepted(3));
    // 送り手の保持は移った。運ばれている間、資源は破棄されない。
    assert_eq!(k.release_object(p, o), Err(Error::Refused(Refusal::NotHeld)));
    assert_eq!(k.release_backing(p, b), Err(Error::Refused(Refusal::NotHeld)));
    assert!(drain(&mut k).is_empty());
    assert_eq!((k.signal_count(o), k.backing_len(b)), (Ok(0), Ok(PAGE)));
    // 受け手が保持を増やせなければ、メッセージは残る。
    let (first, second) = (event(&mut k, q), event(&mut k, q));
    let before = k.snapshot().unwrap();
    assert_eq!(k.receive(q, c, &mut [0u8; 8]), Err(Error::Exhausted(Resource::Holdings)));
    assert_eq!(k, before);
    ok!(k, k.release_object(q, first));
    ok!(k, k.release_object(q, second));
    let _ = drain(&mut k);
    assert_eq!(take(&mut k, q, c, 8), Ok((b"obj".to_vec(), Some(Parcel::Object(o)))));
    assert_eq!(take(&mut k, q, c, 8), Ok((b"mem".to_vec(), Some(Parcel::Backing(b)))));
    assert!(drain(&mut k).is_empty());
    // 受け手が最後の保持者になった。
    ok!(k, k.release_object(q, o));
    ok!(k, k.release_backing(q, b));
    assert_eq!(drain(&mut k), vec![Effect::ObjectReclaimed { object: o }, Effect::BackingReclaimed { backing: b }]);
}

#[test]
fn a_carried_backing_is_released_when_nobody_can_receive_it() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let c = pipe(&mut k, p).channel();
    let b = ok!(k, k.create_backing(p, PAGE));
    assert_eq!(ok!(k, k.send(p, c, b"", Some(Parcel::Backing(b)))), Sent::Accepted(0));
    assert!(drain(&mut k).is_empty());
    ok!(k, k.release_channel(p, c, End::Receive));
    assert_eq!(drain(&mut k), vec![Effect::BackingReclaimed { backing: b }]);
}

#[test]
fn process_exit_closes_its_ends() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (q, _) = process(&mut k);
    let made = pipe(&mut k, p);
    let c = made.channel();
    ok!(k, k.retain_channel(q, c, End::Send));
    ok!(k, k.release_channel(p, c, End::Send));
    assert_eq!(ok!(k, k.send(q, c, b"bye", None)), Sent::Accepted(3));
    ok!(k, k.exit_process(q, ExitCode::new(0)));
    let _ = drain(&mut k);
    // 終了したプロセスの送る端は閉じ、受け手は残りを読んでから終わりを知る。
    assert_eq!(take(&mut k, p, c, 8), Ok((b"bye".to_vec(), None)));
    assert_eq!(take(&mut k, p, c, 8), Err(Received::Ended));
    ok!(k, k.exit_process(p, ExitCode::new(0)));
    assert_eq!(k.census().channels(), 0);
    let _ = drain(&mut k);
    assert!(is_empty(&k));
}

#[test]
fn channels_are_bounded_by_the_limits() {
    let mut k = kernel_with(Limits::DEFAULT.with_channels(2).with_channel_bytes(24));
    let (p, _) = process(&mut k);
    for bad in [ChannelSpec::messages(0, 8), ChannelSpec::stream(0)] {
        assert_eq!(k.create_channel(p, bad), Err(Error::Refused(Refusal::BadChannelSpec)));
    }
    let before = k.snapshot().unwrap();
    assert_eq!(k.create_channel(p, ChannelSpec::stream(25)), Err(Error::Exhausted(Resource::ChannelBytes)));
    assert_eq!(k.create_channel(p, ChannelSpec::messages(5, 5)), Err(Error::Exhausted(Resource::ChannelBytes)));
    assert_eq!(k, before);
    let first = ok!(k, k.create_channel(p, ChannelSpec::messages(2, 8))).channel();
    ok!(k, k.create_channel(p, ChannelSpec::stream(8)));
    assert_eq!(k.create_channel(p, ChannelSpec::stream(1)), Err(Error::Exhausted(Resource::ChannelBytes)));
    assert_eq!(k.create_channel(p, ChannelSpec::messages(1, 0)), Err(Error::Exhausted(Resource::Channels)));
    // 回収すると、数もバイト数も戻る。
    ok!(k, k.release_channel(p, first, End::Send));
    ok!(k, k.release_channel(p, first, End::Receive));
    ok!(k, k.create_channel(p, ChannelSpec::stream(16)));
}

#[test]
fn a_restored_state_carries_the_channel_content() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let c = pipe(&mut k, p).channel();
    let o = event(&mut k, p);
    assert_eq!(ok!(k, k.send(p, c, b"keep", Some(Parcel::Object(o)))), Sent::Accepted(4));
    let mut copy = k.snapshot().unwrap().restore(InstanceId::new(2)).unwrap();
    copy.audit().unwrap();
    assert_eq!(take(&mut copy, p, c, 8), Ok((b"keep".to_vec(), Some(Parcel::Object(o)))));
    // 復元した状態でも、送信は確保済みの容量の中で進む。
    assert_eq!(ok!(copy, copy.send(p, c, &[7; 8], None)), Sent::Accepted(8));
    assert_eq!(ok!(copy, copy.send(p, c, &[8; 8], None)), Sent::Accepted(8));
    assert_eq!(take(&mut k, p, c, 8), Ok((b"keep".to_vec(), Some(Parcel::Object(o)))));
}

// ---- 値の読み出し ----

#[test]
fn accessors_return_what_was_stored() {
    assert_eq!(kernel_with(Limits::DEFAULT).instance(), InstanceId::new(1));
    assert_eq!(InstanceId::new(7).get(), 7);
    assert_eq!(WorkerId::new(5).get(), 5);
    assert_eq!(ExitCode::new(9).get(), 9);
    assert_eq!(HostError::new(7).get(), 7);
}

#[test]
fn the_census_counts_every_kind_of_resource() {
    let mut w = world();
    let (q, _) = process(&mut w.k);
    ok!(w.k, w.k.create_backing(q, PAGE));
    let second = ok!(w.k, w.k.create_timer(q, w.event));
    ok!(w.k, w.k.prepare_io(q, IoLinks::NONE));
    let census = w.k.census();
    assert_eq!(census.processes(), 2);
    assert_eq!(census.threads(), 3);
    // 待機用の資源 1、プロセスの終了通知 2、スレッドの終了通知 3。
    assert_eq!(census.objects(), 6);
    assert_eq!(census.backings(), 2);
    assert_eq!(census.timers(), 2);
    assert_eq!(census.ios(), 2);
    assert_eq!(w.k.thread_count(w.p), Ok(3));
    assert_eq!(w.k.thread_count(q), Ok(0));
    ok!(w.k, w.k.drop_timer(second));
    assert_eq!(w.k.census().timers(), 1);
}

#[test]
fn run_counts_follow_grants() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (first, _) = thread(&mut k, p);
    thread(&mut k, p);
    thread(&mut k, p);
    assert_eq!((k.ready_count(), k.running_count()), (3, 0));
    let _lent = [ok!(k, k.grant(W0)).unwrap(), ok!(k, k.grant(W1)).unwrap()];
    assert_eq!((k.ready_count(), k.running_count()), (1, 2));
    assert_eq!(ok!(k, k.suspend_thread(first)), 0);
    assert_eq!(ok!(k, k.suspend_thread(first)), 1);
    assert_eq!(k.suspend_count(first), Ok(2));
}

#[test]
fn the_map_generation_counts_changes() {
    let mut w = world();
    let before = w.k.map_generation(w.p).unwrap();
    assert_eq!(before.get(), 2);
    ok!(w.k, w.k.unmap(w.p, range(0x2000, PAGE)));
    assert_eq!(w.k.map_generation(w.p).unwrap().get(), 3);
}

// ---- 境界 ----

#[test]
fn registration_numbers_are_distinct_and_increase() {
    let mut w = world();
    let second = ok!(w.k, w.k.create_timer(w.p, w.event));
    ok!(w.k, w.k.arm_timer(second, at(200), None));
    let seqs: Vec<u64> = w.k.due.iter().map(|entry| entry.seq).collect();
    assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
}

#[test]
fn a_deadline_record_that_disagrees_is_an_internal_fault() {
    // 待機の側の期限の記録が、期限の列と食い違っている。
    let mut w = world();
    wait_mut(&mut w.k, w.blocked).due = Some((at(100), 999));
    assert_eq!(w.k.advance(at(150)), Err(Error::Broken(Invariant::TimeOrder)));
    let mut w = world();
    wait_mut(&mut w.k, w.blocked).due = Some((at(100), 999));
    assert_eq!(w.k.terminate_thread(w.blocked, CODE), Err(Error::Broken(Invariant::TimeOrder)));
    // タイマの側の記録が食い違っている。
    let mut w = world();
    w.k.timers.get_mut(w.timer).unwrap().armed.as_mut().unwrap().seq = 999;
    assert_eq!(w.k.advance(at(250)), Err(Error::Broken(Invariant::TimeOrder)));
}

#[test]
fn an_initial_count_may_equal_the_maximum() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let full = ok!(k, k.create_object(p, SignalSpec::new(SignalMode::Consume, 2, 2)));
    assert_eq!(k.signal_count(full), Ok(2));
    assert_eq!(k.create_object(p, SignalSpec::new(SignalMode::Consume, 3, 2)), Err(Error::Refused(Refusal::BadSignalSpec)));
}

#[test]
fn one_object_as_both_target_and_notify_needs_two_pins() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let object = event(&mut k, p);
    k.objects.get_mut(object).unwrap().pins = u32::MAX - 1;
    let before = k.snapshot().unwrap();
    let links = IoLinks::NONE.with_target(object).with_notify(object);
    assert_eq!(k.prepare_io(p, links), Err(Error::Exhausted(Resource::Pins)));
    assert!(k == before, "断った操作が状態を変えた");
}

#[test]
fn a_mapping_must_end_inside_its_backing() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    // 実体が 2 ページに 1 バイト足りない。
    let short = ok!(k, k.create_backing(p, 2 * PAGE - 1));
    assert_eq!(
        k.map(p, range(0x1000, 2 * PAGE), backed(short, 0), Prot::READ),
        Err(Error::Refused(Refusal::BeyondBacking))
    );
    ok!(k, k.map(p, range(0x1000, PAGE), backed(short, 0), Prot::READ));
}

#[test]
fn trimming_a_mapping_needs_no_extra_slot() {
    let limits = Limits::DEFAULT.with_workers(2).with_mappings(2);
    for (start, pages) in [(0x1000, 1), (0x2000, 1), (0x1000, 2)] {
        let mut k = kernel_with(limits);
        let (p, _) = process(&mut k);
        let backing = ok!(k, k.create_backing(p, 4 * PAGE));
        ok!(k, k.map(p, range(0x1000, 2 * PAGE), backed(backing, 0), Prot::READ));
        ok!(k, k.map(p, range(0x4000, 2 * PAGE), backed(backing, 2 * PAGE), Prot::READ));
        // 端を削る・全体を外すのは、マッピングの数を増やさない。
        assert_eq!(ok!(k, k.unmap(p, range(start, pages * PAGE))), pages * PAGE);
        assert!(k.mapping_count(p).unwrap() <= 2);
    }
    // 中をくり抜くと一つ増えるので、上限に当たる。
    let mut k = kernel_with(limits);
    let (p, _) = process(&mut k);
    let backing = ok!(k, k.create_backing(p, 4 * PAGE));
    ok!(k, k.map(p, range(0x1000, 3 * PAGE), backed(backing, 0), Prot::READ));
    ok!(k, k.map(p, range(0x5000, PAGE), backed(backing, 3 * PAGE), Prot::READ));
    assert_eq!(k.unmap(p, range(0x2000, PAGE)), Err(Error::Exhausted(Resource::Mappings)));
}

#[test]
fn a_split_may_fill_the_mapping_limit_exactly() {
    let mut k = kernel_with(Limits::DEFAULT.with_workers(2).with_mappings(3));
    let (p, _) = process(&mut k);
    let backing = ok!(k, k.create_backing(p, 3 * PAGE));
    ok!(k, k.map(p, range(0x1000, 3 * PAGE), backed(backing, 0), Prot::READ));
    ok!(k, k.protect(p, range(0x2000, PAGE), Prot::NONE));
    assert_eq!(k.mapping_count(p), Ok(3));
    // もう一度分けると上限を超える。
    let mut k = kernel_with(Limits::DEFAULT.with_workers(2).with_mappings(2));
    let (p, _) = process(&mut k);
    let backing = ok!(k, k.create_backing(p, 3 * PAGE));
    ok!(k, k.map(p, range(0x1000, 3 * PAGE), backed(backing, 0), Prot::READ));
    assert_eq!(k.protect(p, range(0x2000, PAGE), Prot::NONE), Err(Error::Exhausted(Resource::Mappings)));
}

#[test]
fn splits_count_every_new_reference_to_the_backing() {
    // 中の一ページの権限を変えると、同じ実体を指すマッピングが二つ増える。
    let mut w = world();
    ok!(w.k, w.k.unmap(w.p, range(0x1000, 2 * PAGE)));
    let wide = ok!(w.k, w.k.create_backing(w.p, 3 * PAGE));
    ok!(w.k, w.k.map(w.p, range(0x1000, 3 * PAGE), backed(wide, 0), Prot::READ));
    w.k.backings.get_mut(wide).unwrap().maps = u32::MAX - 1;
    assert_eq!(w.k.protect(w.p, range(0x2000, PAGE), Prot::NONE), Err(Error::Exhausted(Resource::Pins)));
    // 解除でくり抜く場合も、増える前に確かめる。
    w.k.backings.get_mut(wide).unwrap().maps = u32::MAX;
    assert_eq!(w.k.unmap(w.p, range(0x2000, PAGE)), Err(Error::Exhausted(Resource::Pins)));
}

#[test]
fn finishing_a_process_with_threads_is_an_internal_fault() {
    let mut w = world();
    process_mut(&mut w.k, w.p).life = crate::process::Life::Exiting(CODE);
    assert_eq!(w.k.finish_process(w.p), Err(Error::Broken(Invariant::ProcessTree)));
    let mut k = kernel();
    let (p, _) = process(&mut k);
    process_mut(&mut k, p).life = crate::process::Life::Exiting(CODE);
    process_mut(&mut k, p).stale_runners = 1;
    assert_eq!(k.finish_process(p), Err(Error::Broken(Invariant::ProcessTree)));
}

// ---- 復元した状態の予約容量 (レビュー 1.1 の再現例) ----

/// 状態を写し、別の実行インスタンスとして復元する。復元した状態は検査を通る。
fn restored(k: &Kernel) -> Kernel {
    let copy = k.snapshot().unwrap().restore(InstanceId::new(2)).unwrap();
    copy.audit().unwrap();
    copy
}

#[test]
fn a_restored_empty_state_creates_and_exits_a_process() {
    // 再現例: 空の状態を複製し、プロセスを作って終了させると、Effect を出す容量が無かった。
    let mut k = restored(&kernel());
    let (p, p_exit) = process(&mut k);
    ok!(k, k.exit_process(p, CODE));
    assert_eq!(
        drain(&mut k),
        vec![
            Effect::ObjectReclaimed { object: p_exit },
            Effect::ProcessExited { process: p, code: CODE },
            Effect::ProcessReclaimed { process: p },
        ]
    );
    assert!(is_empty(&k));
}

#[test]
fn a_restored_state_lends_the_thread_that_was_ready() {
    // 再現例: 実行待ちのスレッドを持つ状態を複製して実行権を貸すと、貸し出しの表の容量が無く、
    // 実行待ちの列からだけ外れた状態が残った。
    let mut original = kernel();
    let (p, _) = process(&mut original);
    let (t, _) = thread(&mut original, p);
    let mut k = restored(&original);
    let permit = ok!(k, k.grant(W0)).unwrap();
    assert_eq!(permit.thread(), t);
    assert_eq!(permit.instance(), InstanceId::new(2));
    assert_eq!((k.ready_count(), k.running_count()), (0, 1));
    assert_eq!(ok!(k, k.relinquish(permit, Stop::Yielded, Budget::new(1))), ThreadFate::Ready);
    assert_eq!((k.ready_count(), k.running_count()), (1, 0));
    ok!(k, k.exit_process(p, CODE));
    assert!(drain(&mut k).contains(&Effect::ProcessReclaimed { process: p }));
    assert!(is_empty(&k));
}

#[test]
fn a_restored_state_wakes_a_wait_and_reports_it() {
    let mut original = kernel();
    let (p, _) = process(&mut original);
    let (t, _) = thread(&mut original, p);
    let event = event(&mut original, p);
    assert_eq!(trap(&mut original, W0), t);
    assert_eq!(wait_any(&mut original, t, &[event]), WaitStart::Blocked);
    let mut k = restored(&original);
    assert_eq!(ok!(k, k.signal(event, SignalOp::Set(1))), 0);
    assert_eq!(drain(&mut k), vec![Effect::WaitDone { thread: t, outcome: WaitOutcome::Signaled { index: 0 } }]);
    // 起きたスレッドは、続行させると実行待ちの列へ入り、実行権を借りられる。
    assert_eq!(k.thread_phase(t), Ok(ThreadPhase::Servicing));
    ok!(k, k.continue_thread(t));
    assert_eq!(ok!(k, k.grant(W1)).map(|permit| permit.thread()), Some(t));
}

#[test]
fn a_restored_state_refuses_a_permit_lent_by_the_original() {
    // 再現例: 元の状態が貸した実行権を、複製が受け取ってしまった。
    let mut original = kernel();
    let (p, _) = process(&mut original);
    let (t, _) = thread(&mut original, p);
    let mut k = restored(&original);
    let lent_by_original = ok!(original, original.grant(W0)).unwrap();
    // 復元した側も同じスレッドを同じ worker へ貸している (番号も実行の世代も同じ)。
    let own = ok!(k, k.grant(W0)).unwrap();
    assert_eq!((own.thread(), own.run()), (t, lent_by_original.run()));
    let before = k.snapshot().unwrap();
    assert_eq!(k.relinquish(lent_by_original, Stop::Trap, Budget::new(1)).map_err(|refused| refused.error()), Err(Error::Foreign));
    assert_eq!(k, before);
    assert_eq!(ok!(k, k.relinquish(own, Stop::Trap, Budget::new(1))), ThreadFate::Servicing);
}

#[test]
fn lending_without_room_in_the_table_changes_nothing() {
    // 貸し出しの表の容量が無い (内部の障害) とき、実行待ちの列からだけ外すことはしない。
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let _ = thread(&mut k, p);
    k.running.corrupt_capacity();
    let before = k.snapshot().unwrap();
    assert_eq!(k.grant(W0), Err(Error::Broken(Invariant::RunOnePermit)));
    assert_eq!(k, before);
    assert_eq!((k.ready_count(), k.running_count()), (1, 0));
}

// ---- 周期タイマは必ず時刻を進める (レビュー 1.2 の再現例) ----

#[test]
fn the_shortest_period_still_lets_time_advance() {
    // 再現例: 周期の下限を 0 にして周期 0 のタイマを設定すると、時刻を進める操作が終わらなかった。
    // 周期も下限も 0 を表せない (time.rs の compile_fail)。表せる最短の周期でも、期限は毎回後へ進む。
    let mut k = kernel_with(Limits::DEFAULT.with_min_period(Period::MIN));
    let (p, _) = process(&mut k);
    let sem = semaphore(&mut k, p, 0, 1000);
    let timer = ok!(k, k.create_timer(p, sem));
    ok!(k, k.arm_timer(timer, at(0), Some(Period::MIN)));
    assert_eq!(ok!(k, k.advance(at(0))).fired(), 1);
    assert_eq!(k.next_deadline(), Some(at(1)));
    assert_eq!(ok!(k, k.advance(at(9))).fired(), 9);
    assert_eq!((k.now(), k.signal_count(sem), k.next_deadline()), (at(9), Ok(10), Some(at(10))));
}

// ---- 一度の advance の仕事は進めた長さに比例しない (レビュー 3.2 の再現例) ----

fn batch_of(count: u32) -> Limits {
    Limits::DEFAULT.with_min_period(Period::MIN).with_advance_batch(core::num::NonZeroU32::new(count).unwrap())
}

/// `to` に届くまで advance を呼び直し、呼んだ回数と、処理した期限の合計を返す。
fn advance_fully(k: &mut Kernel, to: Instant, batch: u32, crowd: u32) -> (u32, u64) {
    let (mut calls, mut total) = (0u32, 0u64);
    loop {
        let before = k.now();
        let advanced = ok!(k, k.advance(to));
        calls = calls.saturating_add(1);
        total = total.saturating_add(u64::from(advanced.fired()));
        assert!(advanced.fired() <= batch.saturating_add(crowd), "一度の呼び出しが目安を超えて処理した: {advanced:?}");
        assert_eq!(advanced.reached(), k.now());
        assert!(before <= advanced.reached() && advanced.reached() <= to);
        if !advanced.more() {
            assert_eq!(advanced.reached(), to);
            return (calls, total);
        }
        assert!(advanced.fired() >= batch, "目安に届く前に区切った: {advanced:?}");
    }
}

#[test]
fn a_long_jump_returns_after_a_bounded_amount_of_work() {
    // 再現例: 最短の周期のタイマを設定して時刻を表せる端まで進めると、一度の呼び出しが、進めた
    // 長さに比例する回数だけ通知を処理した (終わらないのと変わらない)。
    let mut k = kernel_with(Limits::DEFAULT.with_min_period(Period::MIN));
    let batch = Limits::DEFAULT.advance_batch().get();
    let (p, _) = process(&mut k);
    let sem = semaphore(&mut k, p, 0, u64::MAX);
    let timer = ok!(k, k.create_timer(p, sem));
    ok!(k, k.arm_timer(timer, at(0), Some(Period::MIN)));
    let advanced = ok!(k, k.advance(at(u64::MAX)));
    assert_eq!((advanced.fired(), advanced.more()), (batch, true));
    // 時刻 0 から 1 ずつ batch 回。時刻は最後に処理した期限までしか進まない。
    assert_eq!(advanced.reached(), at(u64::from(batch) - 1));
    assert_eq!((k.now(), k.signal_count(sem), k.next_deadline()), (advanced.reached(), Ok(u64::from(batch)), Some(at(u64::from(batch)))));
    // 続きは同じ時刻で呼び直す。通知は一回も抜けない。
    let advanced = ok!(k, k.advance(at(u64::MAX)));
    assert_eq!((advanced.fired(), advanced.more(), advanced.reached()), (batch, true, at(2 * u64::from(batch) - 1)));
    assert_eq!(k.signal_count(sem), Ok(2 * u64::from(batch)));
    // 残りが無ければ、渡した時刻に届く。
    assert!(ok!(k, k.disarm_timer(timer)));
    let advanced = ok!(k, k.advance(at(u64::MAX)));
    assert_eq!((advanced.fired(), advanced.more(), advanced.reached()), (0, false, at(u64::MAX)));
}

/// タイマ 3 つ (うち 1 つは過去の時刻に設定) と、期限つきの待機 4 つが混ざった状況を、時刻 20 まで
/// 進めたところから 100 まで進め、起きたことを返す。
fn crowded_run(batch: u32) -> (Vec<Effect>, [u64; 3], Option<Instant>, u64, u32) {
    let mut k = kernel_with(batch_of(batch));
    let (p, _) = process(&mut k);
    let threads = [thread(&mut k, p).0, thread(&mut k, p).0, thread(&mut k, p).0, thread(&mut k, p).0];
    let [a, b, c] = [semaphore(&mut k, p, 0, 1000), semaphore(&mut k, p, 0, 1000), semaphore(&mut k, p, 0, 1000)];
    let [on_a, on_b, on_c] = [ok!(k, k.create_timer(p, a)), ok!(k, k.create_timer(p, b)), ok!(k, k.create_timer(p, c))];
    assert!(!ok!(k, k.advance(at(20))).more());
    ok!(k, k.arm_timer(on_a, at(23), Period::new(Duration::from_nanos(2))));
    ok!(k, k.arm_timer(on_b, at(25), Period::new(Duration::from_nanos(5))));
    // 過去の時刻に設定した周期タイマ: 過ぎた 21 回分を、現在の時刻でまとめずに通知する。
    ok!(k, k.arm_timer(on_c, at(0), Some(Period::MIN)));
    let waits: [(&[ObjectId], WaitMode, u64); 4] =
        [(&[a], WaitMode::Any, 50), (&[a, b], WaitMode::All, 24), (&[], WaitMode::Any, 37), (&[b], WaitMode::Any, 37)];
    for (thread, (targets, mode, deadline)) in threads.into_iter().zip(waits) {
        assert_eq!(trap(&mut k, W0), thread);
        let started = k.wait(thread, targets, mode, Deadline::At(at(deadline)), Interruptible::No);
        assert_eq!(ok!(k, started), WaitStart::Blocked);
    }
    let _ = drain(&mut k);
    // 一つの時刻に並ぶ期限は、多くてもタイマ 3 つと待機 4 つ。
    let (calls, total) = advance_fully(&mut k, at(100), batch, 7);
    let counts = [k.signal_count(a).unwrap(), k.signal_count(b).unwrap(), k.signal_count(c).unwrap()];
    (drain(&mut k), counts, k.next_deadline(), total, calls)
}

#[test]
fn splitting_an_advance_changes_no_outcome() {
    // 区切らずに処理した結果 (起床の順序と結果、通知の数、次の期限) と、どの区切り方の結果も同じである。
    let (effects, counts, next, total, calls) = crowded_run(u32::MAX);
    assert_eq!(calls, 1);
    assert!(effects.len() >= 4, "待機が四つとも確定していない: {effects:?}");
    // on_c は時刻 0 から 100 まで 101 回。資源を待つスレッドがいないので、数はそのまま残る。
    assert_eq!(counts[2], 101);
    for batch in [1, 21, 2, 3, 7, 20, 22, 64].into_iter().take(rounds(8).min(if cfg!(miri) { 2 } else { 8 })) {
        let (split_effects, split_counts, split_next, split_total, split_calls) = crowded_run(batch);
        assert_eq!((&split_effects, split_counts, split_next, split_total), (&effects, counts, next, total), "目安 {batch}");
        assert!(split_calls > 1, "目安 {batch} で一度も区切らなかった");
    }
}

#[test]
fn deadlines_of_one_instant_are_never_split() {
    // 目安が 1 でも、同じ時刻に期限が来る待機は一度の呼び出しで全部確定する。一部だけ起こして
    // 返ると、残りは期限を過ぎたまま待ち続けることになる。
    let mut k = kernel_with(batch_of(1));
    let (p, _) = process(&mut k);
    let threads = [thread(&mut k, p).0, thread(&mut k, p).0, thread(&mut k, p).0];
    let e = event(&mut k, p);
    let timer = ok!(k, k.create_timer(p, e));
    for thread in threads {
        assert_eq!(trap(&mut k, W0), thread);
        let started = k.wait(thread, &[], WaitMode::Any, Deadline::At(at(10)), Interruptible::No);
        assert_eq!(ok!(k, started), WaitStart::Blocked);
    }
    ok!(k, k.arm_timer(timer, at(15), None));
    let _ = drain(&mut k);
    let advanced = ok!(k, k.advance(at(20)));
    assert_eq!((advanced.fired(), advanced.reached(), advanced.more()), (3, at(10), true));
    assert_eq!(drain(&mut k), threads.map(|thread| Effect::WaitDone { thread, outcome: WaitOutcome::TimedOut }).to_vec());
    assert_eq!(k.next_deadline(), Some(at(15)));
    let advanced = ok!(k, k.advance(at(20)));
    assert_eq!((advanced.fired(), advanced.reached(), advanced.more()), (1, at(20), false));
    assert_eq!(k.signal_count(e), Ok(1));
}

#[test]
fn catching_up_a_timer_set_in_the_past_is_split_too() {
    // 過去の時刻に設定した周期タイマは、過ぎた回数だけ現在の時刻で通知する。この追いつきも
    // 進めた長さに比例するので、区切って返る。時刻は動かさない。
    let mut k = kernel_with(batch_of(4));
    let (p, _) = process(&mut k);
    let sem = semaphore(&mut k, p, 0, u64::MAX);
    let timer = ok!(k, k.create_timer(p, sem));
    assert!(!ok!(k, k.advance(at(1_000_000))).more());
    ok!(k, k.arm_timer(timer, at(0), Some(Period::MIN)));
    let advanced = ok!(k, k.advance(at(1_000_000)));
    assert_eq!((advanced.fired(), advanced.reached(), advanced.more()), (4, at(1_000_000), true));
    assert_eq!((k.signal_count(sem), k.next_deadline()), (Ok(4), Some(at(4))));
}

// ---- 保留の記録は増え続けない (レビュー 1.3 の再現例) ----

#[test]
fn unmapping_the_same_backing_again_and_again_holds_it_once() {
    // 再現例: マッピング数 1・実体数 1・実体 1 ページの上限のもとで、実行権を返さないスレッドが
    // いる間に同じページの対応づけと解除を 4096 回繰り返すと、保留の記録が 4096 個になった。
    let limits = Limits::DEFAULT.with_workers(2).with_mappings(1).with_backings(1).with_backing_bytes(PAGE);
    let mut k = kernel_with(limits);
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    let b = ok!(k, k.create_backing(p, PAGE));
    ok!(k, k.map(p, range(0x1000, PAGE), backed(b, 0), Prot::READ_WRITE));
    let lent = ok!(k, k.grant(W0)).unwrap();
    for _ in 0..rounds(4096) {
        assert_eq!(ok!(k, k.unmap(p, range(0x1000, PAGE))), PAGE);
        ok!(k, k.map(p, range(0x1000, PAGE), backed(b, 0), Prot::READ_WRITE));
        let space = &k.processes.get(p).unwrap().space;
        assert_eq!(space.limbo.as_slice(), &[b]);
        assert!(space.limbo.capacity() <= 4, "保留の容量が増え続けている: {}", space.limbo.capacity());
        assert_eq!(k.backings.get(b).unwrap().limbo, 1);
    }
    // マッピングも保持も無くしても、古い世代の実行が残る間は解放しない。
    ok!(k, k.unmap(p, range(0x1000, PAGE)));
    ok!(k, k.release_backing(p, b));
    assert_eq!(drain(&mut k), vec![Effect::Interrupt { worker: W0, thread: t, run: lent.run() }]);
    assert_eq!(k.backing_len(b), Ok(PAGE));
    // 最後の古い実行権が返ると、保留が外れて解放される。
    assert_eq!(ok!(k, k.relinquish(lent, Stop::Yielded, Budget::new(1))), ThreadFate::Ready);
    assert_eq!(drain(&mut k), vec![Effect::BackingReclaimed { backing: b }]);
    assert_eq!(k.backing_len(b), Err(Error::Stale(Tag::Backing)));
    assert!(k.processes.get(p).unwrap().space.limbo.is_empty());
}

#[test]
fn two_backings_unmapped_together_are_each_held_once() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let _ = thread(&mut k, p);
    let a = ok!(k, k.create_backing(p, 2 * PAGE));
    let b = ok!(k, k.create_backing(p, PAGE));
    ok!(k, k.map(p, range(0x1000, PAGE), backed(a, 0), Prot::READ));
    ok!(k, k.map(p, range(0x2000, PAGE), backed(b, 0), Prot::READ));
    ok!(k, k.map(p, range(0x3000, PAGE), backed(a, PAGE), Prot::READ));
    let lent = ok!(k, k.grant(W0)).unwrap();
    assert_eq!(ok!(k, k.unmap(p, range(0x1000, 3 * PAGE))), 3 * PAGE);
    assert_eq!(k.processes.get(p).unwrap().space.limbo.as_slice(), &[a, b]);
    assert_eq!((k.backings.get(a).unwrap().limbo, k.backings.get(b).unwrap().limbo), (1, 1));
    ok!(k, k.release_backing(p, a));
    ok!(k, k.release_backing(p, b));
    assert_eq!(ok!(k, k.relinquish(lent, Stop::Yielded, Budget::new(1))), ThreadFate::Ready);
    let effects = drain(&mut k);
    assert!(effects.contains(&Effect::BackingReclaimed { backing: a }));
    assert!(effects.contains(&Effect::BackingReclaimed { backing: b }));
}

// ---- 権限の変更は項目を増やし続けない (レビュー 3.1 の再現例) ----

#[test]
fn protecting_to_the_same_protection_changes_nothing() {
    // 再現例: マッピング数の上限が 1 のとき、3 ページの読み書きマッピングの中央のページへ
    // 同じ権限を設定すると、分割しようとして上限で拒否された。
    let mut k = kernel_with(Limits::DEFAULT.with_workers(2).with_mappings(1));
    let (p, _) = process(&mut k);
    let _ = thread(&mut k, p);
    let b = ok!(k, k.create_backing(p, 3 * PAGE));
    let mapped = ok!(k, k.map(p, range(0x1000, 3 * PAGE), backed(b, 0), Prot::READ_WRITE));
    let lent = ok!(k, k.grant(W0)).unwrap();
    let before = k.snapshot().unwrap();
    assert_eq!(k.protect(p, range(0x2000, PAGE), Prot::READ_WRITE), Ok(mapped));
    // 世代も進まず、実行中のスレッドへの中断も出ない。
    assert_eq!(k, before);
    assert_eq!(k.pending_effects(), 0);
    // 本当に分割が要る変更は、上限で拒否する。
    assert_eq!(k.protect(p, range(0x2000, PAGE), Prot::READ), Err(Error::Exhausted(Resource::Mappings)));
    assert_eq!(k, before);
    assert_eq!(ok!(k, k.relinquish(lent, Stop::Yielded, Budget::new(1))), ThreadFate::Ready);
}

#[test]
fn protect_round_trips_do_not_fragment_a_mapping() {
    // 再現例: 32 MiB の読み書きマッピングで、ページ単位に 読み書き → 読み出しだけ → 読み書き と
    // 戻す操作を繰り返すと、同じ権限の項目が 4095 個に分かれ、次の変更が上限で拒否された。
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let pages = 8192u64;
    let b = ok!(k, k.create_backing(p, pages * PAGE));
    ok!(k, k.map(p, range(0x10_0000, pages * PAGE), backed(b, 0), Prot::READ_WRITE));
    for page in (1..pages - 1).step_by(2).take(rounds(4095)) {
        let one = range(0x10_0000 + page * PAGE, PAGE);
        ok!(k, k.protect(p, one, Prot::READ));
        assert_eq!(k.mapping_count(p), Ok(3));
        ok!(k, k.protect(p, one, Prot::READ_WRITE));
        assert_eq!(k.mapping_count(p), Ok(1));
    }
    assert_eq!(k.backings.get(b).unwrap().maps, 1);
    // ばらばらの権限にしてから全体を同じ権限へ戻すと、一つの項目へ戻る。
    for page in (1..64).step_by(2) {
        ok!(k, k.protect(p, range(0x10_0000 + page * PAGE, PAGE), Prot::READ));
    }
    assert_eq!(k.mapping_count(p), Ok(65));
    ok!(k, k.protect(p, range(0x10_0000, pages * PAGE), Prot::READ_WRITE));
    assert_eq!(k.mapping_count(p), Ok(1));
    assert_eq!(k.backings.get(b).unwrap().maps, 1);
    assert_eq!(read(&k, p, 0x10_0000, pages * PAGE).unwrap().bytes(), pages * PAGE);
}

#[test]
fn protect_counts_the_mappings_left_after_merging() {
    // 上限ちょうどの数で、分割と結合が同時に起きる変更は、結果の数で判定する。
    let mut k = kernel_with(Limits::DEFAULT.with_workers(2).with_mappings(2));
    let (p, _) = process(&mut k);
    let b = ok!(k, k.create_backing(p, 3 * PAGE));
    ok!(k, k.map(p, range(0x1000, 2 * PAGE), backed(b, 0), Prot::READ_WRITE));
    ok!(k, k.map(p, range(0x3000, PAGE), backed(b, 2 * PAGE), Prot::READ));
    // 2 ページ目を読み出しだけにする: 前の項目が分かれ、後ろの項目とまとまる。数は 2 のまま。
    ok!(k, k.protect(p, range(0x2000, PAGE), Prot::READ));
    assert_eq!(k.mapping_count(p), Ok(2));
    assert_eq!(k.mapping_at(p, GuestAddr::new(0x2000)).unwrap().map(|m| m.range()), Some(range(0x2000, 2 * PAGE)));
    // 別名 (実体の中の位置が続かない) や、別の実体とはまとめない。
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let b = ok!(k, k.create_backing(p, 2 * PAGE));
    let c = ok!(k, k.create_backing(p, PAGE));
    ok!(k, k.map(p, range(0x1000, PAGE), backed(b, 0), Prot::READ));
    ok!(k, k.map(p, range(0x2000, PAGE), backed(b, 0), Prot::READ_WRITE));
    ok!(k, k.map(p, range(0x3000, PAGE), backed(c, 0), Prot::READ));
    ok!(k, k.protect(p, range(0x2000, PAGE), Prot::READ));
    assert_eq!(k.mapping_count(p), Ok(3));
}

#[test]
fn only_a_change_that_takes_a_permission_away_stops_running_threads() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    let b = ok!(k, k.create_backing(p, PAGE));
    let mapped = ok!(k, k.map(p, range(0x1000, PAGE), backed(b, 0), Prot::READ));
    let lent = ok!(k, k.grant(W0)).unwrap();
    // 増やすだけ: 世代は進むが、中断は要求しない。
    let widened = ok!(k, k.protect(p, range(0x1000, PAGE), Prot::READ_WRITE));
    assert!(widened > mapped);
    assert!(drain(&mut k).is_empty());
    // 書き込みを足し、読み出しを外す: 減る権限があるので中断を要求する。
    ok!(k, k.protect(p, range(0x1000, PAGE), Prot::new(false, true, true)));
    assert_eq!(drain(&mut k), vec![Effect::Interrupt { worker: W0, thread: t, run: lent.run() }]);
    assert_eq!(ok!(k, k.relinquish(lent, Stop::Yielded, Budget::new(1))), ThreadFate::Ready);
}

// ---- 権限とアクセスの全ての組 (レビュー 4.1) ----

#[test]
fn every_protection_allows_exactly_its_own_accesses() {
    // 権限 8 通り × アクセス 3 通りの期待を、判定の式を使わずに一つずつ書く。
    // 列は (読み出し, 書き込み, 実行) の権限と、読み出し・書き込み・実行のアクセスが通るか。
    const TABLE: [((bool, bool, bool), [bool; 3]); 8] = [
        ((false, false, false), [false, false, false]),
        ((true, false, false), [true, false, false]),
        ((false, true, false), [false, true, false]),
        ((true, true, false), [true, true, false]),
        ((false, false, true), [false, false, true]),
        ((true, false, true), [true, false, true]),
        ((false, true, true), [false, true, true]),
        ((true, true, true), [true, true, true]),
    ];
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let b = ok!(k, k.create_backing(p, PAGE));
    for ((r, w, x), expected) in TABLE {
        let prot = Prot::new(r, w, x);
        ok!(k, k.map(p, range(0x1000, PAGE), backed(b, 0), prot));
        for (access, allowed) in [Access::Read, Access::Write, Access::Execute].into_iter().zip(expected) {
            assert_eq!(prot.allows(access), allowed, "{prot:?} {access:?}");
            let reached = k.translate(p, range(0x1000, 1), access).unwrap();
            assert_eq!(reached.map_err(|fault| fault.kind()).map(|span| span.backing()), if allowed { Ok(b) } else { Err(MemFaultKind::Denied) }, "{prot:?} {access:?}");
        }
        ok!(k, k.unmap(p, range(0x1000, PAGE)));
    }
}

// ---- Effect の取り出しは入力である (レビュー 1.4 の再現例) ----

#[test]
fn the_input_sequence_alone_decides_the_replies() {
    // 再現例: Effect の上限が 4 のとき、資源を作って手放した後の 2 個目の資源の作成は、
    // 間で Effect を取り出したかどうかで結果が変わる。取り出しが入力に無いと、同じ入力の列が
    // 二通りの結果を持ってしまう。
    let run = |polls: bool| {
        let mut k = kernel_with(Limits::DEFAULT.with_effects(4));
        let page = PageSize::new(PAGE).unwrap();
        let spec = SignalSpec::new(SignalMode::Keep, 0, 1);
        let Ok(Reply::Process(p, _)) = k.apply(Input::CreateProcess(AddrWidth::Bits32, page)) else { panic!() };
        let Ok(Reply::Object(object)) = k.apply(Input::CreateObject(p, spec)) else { panic!() };
        let mut replies = vec![k.apply(Input::ReleaseObject(p, object))];
        if polls {
            replies.push(k.apply(Input::PollEffect));
        }
        replies.push(k.apply(Input::CreateObject(p, spec)));
        k.audit().unwrap();
        (replies, k, object)
    };
    let (without, state, _) = run(false);
    assert_eq!(without.last(), Some(&Err(Error::Exhausted(Resource::Effects))));
    let (with, polled, object) = run(true);
    assert_eq!(with.get(1), Some(&Ok(Reply::Effect(Some(Effect::ObjectReclaimed { object })))));
    assert!(matches!(with.last(), Some(Ok(Reply::Object(_)))));
    assert_ne!(state, polled);
    // 同じ入力の列は、同じ返答と同じ状態になる。
    let (again, same, _) = run(true);
    assert_eq!((with, &polled), (again, &same));
    // 空のキューからの取り出しも入力で、何も変えない。
    let mut k = kernel();
    let before = k.snapshot().unwrap();
    assert_eq!(k.apply(Input::PollEffect), Ok(Reply::Effect(None)));
    assert_eq!(k, before);
}

// ---- 届け先を間違えた実行権は失われない (レビュー 2.2 の再現例) ----

#[test]
fn a_permit_given_to_the_wrong_kernel_comes_back() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    let mut other = Kernel::new(InstanceId::new(2), Limits::DEFAULT).unwrap();
    let permit = ok!(k, k.grant(W0)).unwrap();
    let before = other.snapshot().unwrap();
    // 再現例: 別の実行インスタンスへ返すと、拒否はされるが実行権が消え、スレッドは実行中のまま残った。
    let refused = other.relinquish(permit, Stop::Trap, Budget::new(1)).unwrap_err();
    assert_eq!(refused.error(), Error::Foreign);
    assert_eq!(other, before);
    assert_eq!((k.thread_phase(t), k.running_count()), (Ok(ThreadPhase::Running), 1));
    // 取り戻した実行権を、貸した実行インスタンスへ返せる。
    let permit = refused.into_permit().unwrap();
    assert_eq!(ok!(k, k.relinquish(permit, Stop::Trap, Budget::new(1))), ThreadFate::Servicing);
    assert_eq!((k.thread_phase(t), k.running_count()), (Ok(ThreadPhase::Servicing), 0));
    assert_eq!(k.grant(W0), Ok(None), "返した worker がまだ実行権を持っている扱いになった");
}

#[test]
fn a_failure_after_acceptance_consumes_the_permit() {
    // 受理した後の内部の障害 (ここでは、貸し出しの記録が消えている) は、拒否と区別して報告する。
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let _ = thread(&mut k, p);
    let permit = ok!(k, k.grant(W0)).unwrap();
    assert!(k.running.pop().is_some());
    let failed = k.relinquish(permit, Stop::Trap, Budget::new(1)).unwrap_err();
    assert_eq!(failed, NotRelinquished::Failed(Error::Broken(Invariant::RunOnePermit)));
    assert_eq!(failed.error(), Error::Broken(Invariant::RunOnePermit));
    assert_eq!(failed.into_permit(), None);
}

// ---- 資源の ID は発行元を持つ (レビュー 2.1 の再現例) ----

/// 同じ手順で作った二つの状態。ID のスロットと世代は同じで、発行元だけが違う。
struct Issued {
    k: Kernel,
    p: ProcessId,
    t: ThreadId,
    o: ObjectId,
    b: BackingId,
    timer: TimerId,
    io: IoId,
    c: ChannelId,
}

fn pair_member(instance: u64) -> Issued {
    let mut k = Kernel::new(InstanceId::new(instance), Limits::DEFAULT.with_workers(2)).unwrap();
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    assert_eq!(trap(&mut k, W0), t);
    let o = event(&mut k, p);
    let b = ok!(k, k.create_backing(p, 2 * PAGE));
    ok!(k, k.map(p, range(0x1000, PAGE), backed(b, 0), Prot::READ));
    let timer = ok!(k, k.create_timer(p, o));
    let io = ok!(k, k.prepare_io(p, IoLinks::NONE));
    let c = ok!(k, k.create_channel(p, ChannelSpec::messages(2, 8))).channel();
    Issued { k, p, t, o, b, timer, io, c }
}

#[test]
fn a_resource_id_from_another_kernel_is_foreign() {
    // 再現例: 二つの実行インスタンスで、片方が発行した実体の ID をもう片方の解放へ渡すと成功し、
    // 関係のない実体が解放された。別のインスタンスのプロセスの ID での対応づけも成功した。
    let mut first = pair_member(1);
    let mut second = pair_member(2);
    assert_eq!((first.b.slot(), first.b.generation()), (second.b.slot(), second.b.generation()));
    assert_ne!(first.b, second.b);
    assert_eq!((first.b.issuer(), second.b.issuer()), (InstanceId::new(1), InstanceId::new(2)));
    let before = second.k.snapshot().unwrap();
    assert_eq!(second.k.release_backing(second.p, first.b), Err(Error::Foreign));
    assert_eq!(second.k.map(first.p, range(0x8000, PAGE), MapSource::Reserved, Prot::NONE), Err(Error::Foreign));
    assert_eq!(second.k, before);
    assert_eq!(second.k.backing_len(second.b), Ok(2 * PAGE));
    // 自分の ID は使える。
    ok!(first.k, first.k.release_backing(first.p, first.b));
}

#[test]
fn a_foreign_id_is_refused_in_every_position() {
    // どの操作のどの引数でも、別の実行インスタンスの ID は、状態を変えずに Foreign で拒否する。
    // lint foreign-ids-tested が、Input の ID の引数が全てここにあることを確かめる。
    let mut own = pair_member(1);
    let other = pair_member(2);
    let (p, t, o, b, timer, io, c) = (own.p, own.t, own.o, own.b, own.timer, own.io, own.c);
    let (fp, ft, fo, fb, ftimer, fio, fc) = (other.p, other.t, other.o, other.b, other.timer, other.io, other.c);
    let mut buffers = [[0u8; 8]; 2];
    let [first_buffer, second_buffer] = &mut buffers;
    let pipe = ChannelSpec::stream(8);
    let spec = SignalSpec::new(SignalMode::Keep, 0, 1);
    let code = ExitCode::new(1);
    let page = range(0x8000, PAGE);
    let done = Completion::new(InstanceId::new(1), CompletionKind::Io { io: fio, outcome: IoOutcome::Done { transferred: 0 } });
    let foreign_target = [fo];
    let own_target = [o];
    let inputs: Vec<Input<'_>> = vec![
        Input::ExitProcess(fp, code),
        Input::CreateThread(fp, Start::Ready),
        Input::ContinueThread(ft),
        Input::SuspendThread(ft),
        Input::UnsuspendThread(ft),
        Input::TerminateThread(ft, code),
        Input::GrantThread(W0, ft),
        Input::PreemptThread(ft),
        Input::SetPriority(ft, Priority::DEFAULT),
        Input::CreateObject(fp, spec),
        Input::RetainObject(fp, o),
        Input::RetainObject(p, fo),
        Input::ReleaseObject(fp, o),
        Input::ReleaseObject(p, fo),
        Input::Signal(fo, SignalOp::Set(1)),
        Input::Wait(ft, &own_target, WaitMode::Any, Deadline::Never, Interruptible::No),
        Input::Wait(t, &foreign_target, WaitMode::Any, Deadline::Never, Interruptible::No),
        Input::InterruptWait(ft),
        Input::CreateTimer(fp, o),
        Input::CreateTimer(p, fo),
        Input::ArmTimer(ftimer, at(5), None),
        Input::DisarmTimer(ftimer),
        Input::DropTimer(ftimer),
        Input::PrepareIo(fp, IoLinks::NONE),
        Input::PrepareIo(p, IoLinks::NONE.with_target(fo)),
        Input::PrepareIo(p, IoLinks::NONE.with_notify(fo)),
        Input::IssueIo(fio),
        Input::CancelIo(fio),
        Input::Complete(done),
        Input::PublishIo(fio),
        Input::ReleaseIo(fio),
        Input::CreateBacking(fp, PAGE),
        Input::RetainBacking(fp, b),
        Input::RetainBacking(p, fb),
        Input::ReleaseBacking(fp, b),
        Input::ReleaseBacking(p, fb),
        Input::Map(fp, page, MapSource::Reserved, Prot::NONE),
        Input::Map(p, page, backed(fb, 0), Prot::READ),
        Input::Unmap(fp, range(0x1000, PAGE)),
        Input::Protect(fp, range(0x1000, PAGE), Prot::READ_WRITE),
        Input::CreateChannel(fp, pipe),
        Input::RetainChannel(fp, c, End::Send),
        Input::RetainChannel(p, fc, End::Send),
        Input::ReleaseChannel(fp, c, End::Receive),
        Input::ReleaseChannel(p, fc, End::Receive),
        Input::Send(fp, c, b"a", None),
        Input::Send(p, fc, b"a", None),
        Input::Send(p, c, b"a", Some(Parcel::Object(fo))),
        Input::Send(p, c, b"a", Some(Parcel::Backing(fb))),
        Input::Receive(fp, c, first_buffer),
        Input::Receive(p, fc, second_buffer),
    ];
    let before = own.k.snapshot().unwrap();
    for (at, input) in inputs.into_iter().enumerate() {
        let shown = format!("{input:?}");
        assert_eq!(own.k.apply(input), Err(Error::Foreign), "{at}: {shown}");
        assert_eq!(own.k, before, "{at}: {shown} が状態を変えた");
    }
    // 状態を変えない問い合わせも同じ。
    let k = &own.k;
    let addr = GuestAddr::new(0x1000);
    let unit = PageSize::new(PAGE).unwrap();
    assert_eq!(k.process_phase(fp), Err(Error::Foreign));
    assert_eq!(k.thread_count(fp), Err(Error::Foreign));
    assert_eq!(k.thread_phase(ft), Err(Error::Foreign));
    assert_eq!(k.priority(ft), Err(Error::Foreign));
    assert_eq!(k.suspend_count(ft), Err(Error::Foreign));
    assert_eq!(k.process_of(ft), Err(Error::Foreign));
    assert_eq!(k.executed(ft), Err(Error::Foreign));
    assert_eq!(k.signal_count(fo), Err(Error::Foreign));
    assert_eq!(k.backing_len(fb), Err(Error::Foreign));
    assert_eq!(k.io_phase(fio), Err(Error::Foreign));
    assert_eq!(k.io_outcome(fio), Err(Error::Foreign));
    assert_eq!(k.channel_queued(fc), Err(Error::Foreign));
    assert_eq!(k.next_message_len(fc), Err(Error::Foreign));
    assert_eq!(k.translate(fp, range(0x1000, 1), Access::Read), Err(Error::Foreign));
    assert_eq!(k.mapping_at(fp, addr), Err(Error::Foreign));
    assert_eq!(k.next_mapping(fp, addr), Err(Error::Foreign));
    assert_eq!(k.find_free(fp, PAGE, unit, range(0, 0x10_0000)), Err(Error::Foreign));
    assert_eq!(k.map_generation(fp), Err(Error::Foreign));
    assert_eq!(k.mapping_count(fp), Err(Error::Foreign));
    // 自分の ID は、同じ引数の位置で受理される (拒否の理由が ID であることの対照)。
    ok!(own.k, own.k.retain_backing(p, b));
    ok!(own.k, own.k.arm_timer(timer, at(5), None));
    ok!(own.k, own.k.issue_io(io));
    assert_eq!(wait_any(&mut own.k, t, &own_target), WaitStart::Blocked);
}

#[test]
fn a_restored_state_keeps_the_ids_issued_before_the_snapshot_only() {
    let mut original = pair_member(1);
    let snapshot = original.k.snapshot().unwrap();
    let mut copy = snapshot.restore(InstanceId::new(2)).unwrap();
    // 写しの時点までの ID は、復元した状態でもそのまま使える。
    assert_eq!(copy.backing_len(original.b), Ok(2 * PAGE));
    assert_eq!(copy.thread_phase(original.t), Ok(ThreadPhase::Servicing));
    // 写しの後に、両方が同じスロット・同じ世代の資源を作る。発行元が違うので、取り違えない。
    let late = event(&mut original.k, original.p);
    let fresh = event(&mut copy, original.p);
    assert_eq!((late.slot(), late.generation()), (fresh.slot(), fresh.generation()));
    assert_eq!((late.issuer(), fresh.issuer()), (InstanceId::new(1), InstanceId::new(2)));
    // 復元元が後から発行した ID は、復元した状態には無い資源である (発行元は祖先なので Stale)。
    assert_eq!(copy.signal_count(late), Err(Error::Stale(Tag::Object)));
    assert_eq!(copy.signal_count(fresh), Ok(0));
    // 復元した状態が発行した ID は、復元元にとっては別の実行インスタンスのもの。
    assert_eq!(original.k.signal_count(fresh), Err(Error::Foreign));
    // 解放して同じスロットを使い回しても、古い ID は古いままである。
    ok!(copy, copy.release_object(original.p, original.o));
    assert_eq!(copy.signal_count(original.o), Ok(0), "タイマが固定している資源を破棄した");
    ok!(copy, copy.drop_timer(original.timer));
    assert_eq!(copy.signal_count(original.o), Err(Error::Stale(Tag::Object)));
    let reused = event(&mut copy, original.p);
    assert_eq!(reused.slot(), original.o.slot());
    assert_eq!(copy.signal_count(original.o), Err(Error::Stale(Tag::Object)));
    // 孫の世代でも、祖先が発行した ID を引き継ぐ。
    let grandchild = copy.snapshot().unwrap().restore(InstanceId::new(3)).unwrap();
    assert_eq!(grandchild.backing_len(original.b), Ok(2 * PAGE));
    assert_eq!(grandchild.signal_count(fresh), Ok(0));
    assert_eq!(grandchild.signal_count(late), Err(Error::Stale(Tag::Object)));
    assert_eq!(copy.audit(), Ok(()));
    assert_eq!(grandchild.audit(), Ok(()));
}

#[test]
fn each_stop_reason_decides_where_the_thread_goes() {
    let fault = MemFault::new(GuestAddr::new(0x10), MemFaultKind::Unmapped);
    let quantum = Limits::DEFAULT.quantum();
    let cases = [
        (Stop::BudgetExhausted, ThreadFate::Ready),
        (Stop::Yielded, ThreadFate::Ready),
        (Stop::Trap, ThreadFate::Servicing),
        (Stop::Fault(fault), ThreadFate::Servicing),
        (Stop::Exited(ExitCode::new(5)), ThreadFate::Exited(ExitCode::new(5))),
    ];
    for (stop, fate) in cases {
        let mut k = kernel();
        let (p, _) = process(&mut k);
        let (t, _) = thread(&mut k, p);
        let (_other, _) = thread(&mut k, p);
        let permit = ok!(k, k.grant(W0)).unwrap();
        assert_eq!(ok!(k, k.relinquish(permit, stop, quantum)), fate, "{stop:?}");
        match fate {
            // 実行待ちへ戻ったスレッドは、列の末尾にいる。
            ThreadFate::Ready => assert_eq!((k.ready_thread(1), k.thread_phase(t)), (Some(t), Ok(ThreadPhase::Ready))),
            ThreadFate::Servicing => assert_eq!(k.thread_phase(t), Ok(ThreadPhase::Servicing)),
            ThreadFate::Exited(code) => {
                assert_eq!(k.thread_phase(t), Err(Error::Stale(Tag::Thread)));
                assert!(drain(&mut k).contains(&Effect::ThreadExited { thread: t, process: p, code }));
            }
        }
    }
    // 中断の要求に応じた返却は、実行待ちへ戻る。停止要求があれば、列には入らない。
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    let permit = ok!(k, k.grant(W0)).unwrap();
    ok!(k, k.suspend_thread(t));
    assert_eq!(ok!(k, k.relinquish(permit, Stop::Interrupted, Budget::new(1))), ThreadFate::Ready);
    assert_eq!((k.ready_count(), ok!(k, k.grant(W0))), (0, None));
    // 実行中に受けた終了要求は、どの理由よりも先に効く。
    let (u, _) = thread(&mut k, p);
    let permit = ok!(k, k.grant(W0)).unwrap();
    ok!(k, k.terminate_thread(u, ExitCode::new(9)));
    assert_eq!(ok!(k, k.relinquish(permit, Stop::Exited(ExitCode::new(1)), Budget::new(0))), ThreadFate::Exited(ExitCode::new(9)));
}

#[test]
fn a_stop_that_does_not_fit_the_run_is_refused_and_the_permit_comes_back() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    let quantum = Limits::DEFAULT.quantum();
    let permit = ok!(k, k.grant(W0)).unwrap();
    let before = k.snapshot().unwrap();
    // 貸した量に届いていないのに、使い切ったとは言えない。
    let short = Budget::new(quantum.get() - 1);
    let refused = k.relinquish(permit, Stop::BudgetExhausted, short).unwrap_err();
    assert_eq!(refused.error(), Error::Refused(Refusal::BudgetRemains));
    assert_eq!(k, before);
    // 要求されていない中断に応じたとは言えない。
    let permit = refused.into_permit().unwrap();
    let refused = k.relinquish(permit, Stop::Interrupted, quantum).unwrap_err();
    assert_eq!(refused.error(), Error::Refused(Refusal::NotInterrupted));
    assert_eq!(k, before);
    // 拒否された実行権は失われていない。要求があれば、同じ理由が通る。
    let permit = refused.into_permit().unwrap();
    ok!(k, k.preempt_thread(t));
    assert_eq!(ok!(k, k.relinquish(permit, Stop::Interrupted, Budget::new(0))), ThreadFate::Ready);
    // ちょうど貸した量と、区切りの都合でそれを超えた量は、使い切ったと言える。
    for consumed in [quantum, Budget::new(quantum.get() + 7)] {
        let permit = ok!(k, k.grant(W0)).unwrap();
        assert_eq!(ok!(k, k.relinquish(permit, Stop::BudgetExhausted, consumed)), ThreadFate::Ready);
    }
}

#[test]
fn the_highest_priority_runs_first_and_equals_run_in_order() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (a, _) = thread(&mut k, p);
    let (b, _) = thread(&mut k, p);
    let (c, _) = thread(&mut k, p);
    let (d, _) = thread(&mut k, p);
    assert_eq!(k.priority(a), Ok(Priority::DEFAULT));
    let high = Priority::new(Priority::DEFAULT.get() + 1);
    let low = Priority::new(Priority::DEFAULT.get() - 1);
    assert_eq!(ok!(k, k.set_priority(c, high)), Priority::DEFAULT);
    assert_eq!(ok!(k, k.set_priority(a, low)), Priority::DEFAULT);
    // 待ち順は a b c d。貸す順は、優先度の高い c、同じ優先度の中で先に待っていた b、d、最後に a。
    let mut order = Vec::new();
    for _ in 0..4 {
        let permit = ok!(k, k.grant(W0)).unwrap();
        order.push(permit.thread());
        assert_eq!(ok!(k, k.relinquish(permit, Stop::Trap, Budget::new(0))), ThreadFate::Servicing);
    }
    assert_eq!(order, vec![c, b, d, a]);
    // 優先度を変えても、列の中の待ち順は変わらない。同じ値に戻せば、先に待っていた方が先。
    for t in [a, b] {
        ok!(k, k.continue_thread(t));
    }
    ok!(k, k.set_priority(a, Priority::DEFAULT));
    assert_eq!((k.ready_thread(0), k.ready_thread(1), k.ready_thread(2)), (Some(a), Some(b), None));
    let permit = ok!(k, k.grant(W0)).unwrap();
    assert_eq!(permit.thread(), a);
    // 優先度の高いスレッドが実行待ちでも、実行中のスレッドを kome が自分で止めることは無い。
    ok!(k, k.continue_thread(c));
    assert_eq!(drain(&mut k), vec![]);
    // 譲らせるのは Personality の判断で、中断の要求は一度の実行に一度だけ出る。
    ok!(k, k.preempt_thread(a));
    ok!(k, k.preempt_thread(a));
    assert_eq!(drain(&mut k), vec![Effect::Interrupt { worker: W0, thread: a, run: permit.run() }]);
    assert_eq!(k.preempt_thread(b), Err(Error::Refused(Refusal::NotRunning)));
    assert_eq!(ok!(k, k.relinquish(permit, Stop::Interrupted, Budget::new(3))), ThreadFate::Ready);
    assert_eq!(ok!(k, k.grant(W0)).map(|permit| permit.thread()), Some(c));
}

#[test]
fn a_named_grant_ignores_the_order_but_not_the_rules() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (a, _) = thread(&mut k, p);
    let (b, _) = thread(&mut k, p);
    let made = k.create_thread(p, Start::Suspended);
    let (held_back, _) = ok!(k, made);
    // 待ち順でも優先度でも a が先だが、名指しした b へ貸す。
    ok!(k, k.set_priority(a, Priority::new(255)));
    let named = ok!(k, k.grant_thread(W0, b));
    assert_eq!((named.thread(), k.thread_phase(b)), (b, Ok(ThreadPhase::Running)));
    let before = k.snapshot().unwrap();
    // 実行中、停止要求つき、同じ worker、存在しないスレッドへは貸さない。状態も変えない。
    assert_eq!(k.grant_thread(W1, b).map(|_| ()), Err(Error::Refused(Refusal::NotReady)));
    assert_eq!(k.grant_thread(W1, held_back).map(|_| ()), Err(Error::Refused(Refusal::NotReady)));
    assert_eq!(k.grant_thread(W0, a).map(|_| ()), Err(Error::Refused(Refusal::WorkerBusy)));
    assert_eq!(k, before);
    // 権限を減らす変更の後、古い世代の実行が残るプロセスのスレッドは、名指ししても貸さない。
    let backing = ok!(k, k.create_backing(p, PAGE));
    ok!(k, k.map(p, range(0x1000, PAGE), backed(backing, 0), Prot::READ_WRITE));
    ok!(k, k.unmap(p, range(0x1000, PAGE)));
    assert_eq!(k.grant_thread(W1, a).map(|_| ()), Err(Error::Refused(Refusal::Barrier)));
    assert_eq!(ok!(k, k.relinquish(named, Stop::Interrupted, Budget::new(0))), ThreadFate::Ready);
    let second = ok!(k, k.grant_thread(W1, a));
    assert_eq!(second.map(), k.map_generation(p).unwrap());
    let _ = k.relinquish(second, Stop::Trap, Budget::new(0));
}

/// 文脈の数を一つ進めるだけの実行器。止まる理由は、決めた順に返す。
struct Counter {
    stops: Vec<Stop>,
    runs: Vec<(WorkerId, ThreadId, u32)>,
}

impl Executor for Counter {
    type Context = u32;

    fn run(&mut self, permit: &RunPermit, context: &mut u32) -> Stopped {
        *context = context.saturating_add(1);
        self.runs.push((permit.worker(), permit.thread(), *context));
        Stopped::new(self.stops.pop().unwrap_or(Stop::Yielded), permit.budget())
    }
}

#[test]
fn a_context_follows_its_thread_across_workers() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let mut contexts = Contexts::new();
    let mut threads = Vec::new();
    for start in [100, 200, 300] {
        let (t, _) = thread(&mut k, p);
        contexts.insert(t, start).unwrap();
        assert_eq!(contexts.insert(t, 0), Err(Error::Refused(Refusal::ContextBusy)));
        threads.push(t);
    }
    let (a, b, c) = (threads[0], threads[1], threads[2]);
    // 二つの worker が、三つのスレッドを交代で実行する。文脈はスレッドに付いて回る。
    let mut executor = Counter { stops: Vec::new(), runs: Vec::new() };
    for worker in [W0, W1, W1, W0, W1, W0] {
        let ran = ok!(k, run_once(&mut k, worker, &mut executor, &mut contexts)).unwrap();
        assert_eq!((ran.stopped().stop(), ran.fate()), (Stop::Yielded, ThreadFate::Ready));
    }
    let expected = vec![(W0, a, 101), (W1, b, 201), (W1, c, 301), (W0, a, 102), (W1, b, 202), (W0, c, 302)];
    assert_eq!(executor.runs, expected);
    // 取り出している間は、同じスレッドの文脈をもう一度取り出せない。
    let permit = ok!(k, k.grant(W0)).unwrap();
    let loaded = contexts.load(&permit).unwrap();
    assert_eq!((loaded.thread(), contexts.load(&permit).map(|_| ())), (a, Err(Error::Refused(Refusal::ContextBusy))));
    contexts.save(loaded).unwrap();
    assert_eq!(ok!(k, k.relinquish(permit, Stop::Yielded, Budget::new(0))), ThreadFate::Ready);
    // 終了まで実行したスレッドの文脈は取り除く。残りは実行を続ける。
    executor.stops = vec![Stop::Exited(ExitCode::new(4))];
    let ran = ok!(k, run_once(&mut k, W0, &mut executor, &mut contexts)).unwrap();
    assert_eq!((ran.thread(), ran.fate(), contexts.len()), (b, ThreadFate::Exited(ExitCode::new(4)), 2));
    assert_eq!(contexts.remove(b), Err(Error::Refused(Refusal::NoContext)));
    assert_eq!(contexts.remove(c), Ok(Some(302)));
    // 貸せるスレッドが無ければ、何もしない。
    for t in [a, c] {
        ok!(k, k.suspend_thread(t));
    }
    assert_eq!(ok!(k, run_once(&mut k, W0, &mut executor, &mut contexts)), None);
}

#[test]
fn a_run_that_cannot_proceed_still_returns_its_permit() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (t, _) = thread(&mut k, p);
    let mut contexts = Contexts::new();
    let mut executor = Counter { stops: Vec::new(), runs: Vec::new() };
    // 文脈の無いスレッド: 実行せず、実行権を返してサービス中にし、失敗を返す。
    assert_eq!(run_once(&mut k, W0, &mut executor, &mut contexts), Err(Error::Refused(Refusal::NoContext)));
    assert_eq!((k.running_count(), k.thread_phase(t), executor.runs.len()), (0, Ok(ThreadPhase::Servicing), 0));
    k.audit().unwrap();
    // 状況と合わない理由を返した実行器: 文脈は戻っており、実行権も返っている。
    contexts.insert(t, 7).unwrap();
    ok!(k, k.continue_thread(t));
    executor.stops = vec![Stop::Interrupted];
    assert_eq!(run_once(&mut k, W0, &mut executor, &mut contexts), Err(Error::Refused(Refusal::NotInterrupted)));
    assert_eq!((k.running_count(), k.thread_phase(t)), (0, Ok(ThreadPhase::Servicing)));
    k.audit().unwrap();
    // 実行の途中でスレッドの文脈を取り除くと、後から戻した文脈は捨てられる。
    ok!(k, k.continue_thread(t));
    let permit = ok!(k, k.grant(W0)).unwrap();
    let loaded = contexts.load(&permit).unwrap();
    assert_eq!(contexts.remove(t), Ok(None));
    assert_eq!(contexts.save(loaded), Err(Error::Refused(Refusal::NoContext)));
    assert!(contexts.is_empty());
    let _ = k.relinquish(permit, Stop::Trap, Budget::new(0));
}
