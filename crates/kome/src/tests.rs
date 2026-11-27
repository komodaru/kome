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
    assert_eq!(ok!(kernel, kernel.relinquish(permit, Stop::Trapped, Budget::new(1))), ThreadFate::Servicing);
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
    assert_eq!(ok!(k, k.relinquish(permit, Stop::Preempted, Budget::new(5))), ThreadFate::Exited(ExitCode::new(9)));
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
    assert_eq!(ok!(k, k.relinquish(first, Stop::Preempted, Budget::new(100_003))), ThreadFate::Ready);
    assert_eq!(k.executed(a), Ok(100_003), "区切りの検査で貸した量を超えた実行量を数える");
    let second = ok!(k, k.grant(W0)).unwrap();
    assert_eq!(second.thread(), b);
    let third = ok!(k, k.grant(W1)).unwrap();
    assert_eq!((third.thread(), third.run().get()), (a, 2));
    assert_eq!(k.grant(WorkerId::new(2)), Err(Error::Exhausted(Resource::Workers)));
    let _ = k.relinquish(second, Stop::Trapped, Budget::new(0));
    let _ = k.relinquish(third, Stop::Trapped, Budget::new(0));
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
    let _ = k.relinquish(permit, Stop::Trapped, Budget::new(0));
}

#[test]
fn a_stale_or_foreign_permit_changes_nothing() {
    let mut k = kernel();
    let (p, _) = process(&mut k);
    let (_t, _) = thread(&mut k, p);
    let first = ok!(k, k.grant(W0)).unwrap();
    // 同じ貸し出しを覚えている別の状態 (復元前の状態に相当)。
    let mut twin = k.clone();
    let mut other = Kernel::new(InstanceId::new(2), Limits::DEFAULT).unwrap();
    assert_eq!(ok!(k, k.relinquish(first, Stop::Preempted, Budget::new(1))), ThreadFate::Ready);
    let second = ok!(k, k.grant(W0)).unwrap();
    let before = twin.clone();
    // twin は 1 回目の実行を貸したままなので、2 回目の実行権は古い。
    assert_eq!(twin.relinquish(second, Stop::Trapped, Budget::new(1)), Err(Error::Refused(Refusal::StalePermit)));
    assert_eq!(twin, before);
    let third = twin.grant(W1);
    assert_eq!(third, Ok(None));
    let (op, _) = process(&mut other);
    let _ = thread(&mut other, op);
    let foreign = other.grant(W0).unwrap().unwrap();
    let before = k.clone();
    assert_eq!(k.relinquish(foreign, Stop::Trapped, Budget::new(1)), Err(Error::Foreign));
    assert_eq!(k, before);
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
    assert_eq!(ok!(k, k.relinquish(permit, Stop::Preempted, Budget::new(1))), ThreadFate::Ready);
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
    let before = k.clone();
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
    assert_eq!(ok!(k, k.advance(at(99))), 0);
    assert_eq!(ok!(k, k.advance(at(100))), 1);
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
        assert_eq!(ok!(k, k.advance(to)), 1);
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
    assert_eq!(ok!(k, k.advance(at(10))), 2, "タイマで起きた待機の期限まで処理した");
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
    let period = Limits::DEFAULT.min_period();
    assert_eq!(
        k.arm_timer(timer, at(5), Some(Duration::from_nanos(period.nanos() - 1))),
        Err(Error::Refused(Refusal::PeriodTooShort))
    );
    ok!(k, k.arm_timer(timer, at(5), Some(period)));
    assert_eq!(ok!(k, k.advance(at(5 + 2 * period.nanos()))), 3);
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
    ok!(k, k.advance(at(1000)));
    ok!(k, k.arm_timer(timer, at(10), None));
    assert_eq!(ok!(k, k.advance(at(1000))), 1);
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
    assert_eq!(ok!(k, k.advance(at(u64::MAX))), 1);
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
    let before = k.clone();
    assert_eq!(k.complete(foreign), Err(Error::Foreign));
    assert_eq!(k, before);
    ok!(k, k.complete(completion(&k, io, DONE)));
    let before = k.clone();
    let again = completion(&k, io, IoOutcome::Failed { error: HostError::new(5), transferred: 0 });
    assert_eq!(k.complete(again), Err(Error::Refused(Refusal::IoSettled)), "終端済みの I/O が再度終端した");
    assert_eq!(k, before);
    assert_eq!(k.io_outcome(io), Ok(Some(DONE)));
    ok!(k, k.release_io(io));
    let before = k.clone();
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
    let before = k.clone();
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
    let before = k.clone();
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
    let before = k.clone();
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
    let before = k.clone();
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
    ok!(k, k.protect(p, range(0x13000, 2 * PAGE), Prot::NONE));
    assert_eq!(k.mapping_count(p), Ok(6));
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
    let _ = k.relinquish(unrelated, Stop::Trapped, Budget::new(0));
    assert_eq!(ok!(k, k.relinquish(old, Stop::Preempted, Budget::new(1))), ThreadFate::Ready);
    assert_eq!(drain(&mut k), vec![Effect::BackingReclaimed { backing: b }]);
    let fresh = ok!(k, k.grant(W0)).unwrap();
    assert_eq!((fresh.thread(), fresh.map()), (caller, k.map_generation(p).unwrap()));
    let _ = k.relinquish(fresh, Stop::Trapped, Budget::new(0));
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
    let _ = k.relinquish(permit, Stop::Trapped, Budget::new(0));
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
    let before = k.clone();
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
    assert_eq!(by_input.apply(Input::Relinquish(permit, Stop::Trapped, Budget::new(2))), Ok(Reply::Fate(ThreadFate::Servicing)));
    by_call.relinquish(other, Stop::Trapped, Budget::new(2)).unwrap();
    let reply = by_input.apply(Input::Wait(t, &[], WaitMode::Any, Deadline::At(at(9)), Interruptible::No));
    assert_eq!(reply, Ok(Reply::Wait(WaitStart::Blocked)));
    by_call.wait(t, &[], WaitMode::Any, Deadline::At(at(9)), Interruptible::No).unwrap();
    assert_eq!(by_input.apply(Input::Advance(at(9))), Ok(Reply::Count(1)));
    by_call.advance(at(9)).unwrap();
    assert_eq!(by_input, by_call);
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
    w.k.backings.remove(w.backing);
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
