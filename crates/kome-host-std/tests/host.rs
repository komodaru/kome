//! 実スレッドで参照ホストを動かす。
#![cfg(not(loom))]

#[cfg(test)]
mod support;

use std::thread;
use std::time::Instant as StdInstant;

use kome::{
    BackingOffset, Contents, Executor, Instant, RunPermit, SignalMode, SignalSpec, Stop, Stopped, ThreadFate,
    ThreadPhase, WorkerId,
};
use kome_host_std::{FlatStore, HostFault, Interrupt, fault};

fn clock() -> impl Fn() -> Instant + Send + Sync + 'static {
    let origin = StdInstant::now();
    move || Instant::from_nanos(u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX))
}

#[test]
fn workers_run_every_thread_to_its_end() {
    for (workers, threads, yields) in [(1, 1, 0), (1, 5, 3), (4, 16, 50), (8, 3, 200)] {
        let (host, _, made) = support::host(workers, threads, yields, clock());
        thread::scope(|scope| {
            let running: Vec<_> = (0..workers)
                .map(|worker| {
                    let host = host.clone();
                    scope.spawn(move || host.work(WorkerId::new(worker), &mut support::Stepper))
                })
                .collect();
            host.wait_until(|state| state.personality.process_exited).unwrap();
            host.close().unwrap();
            for worker in running {
                assert_eq!(worker.join().unwrap(), Ok(()));
            }
        });
        support::check(&host, &made, yields);
    }
}

/// 割り込みが求められるまでゲストを実行し続ける実行器。
struct Spinner {
    interrupt: Interrupt,
}

impl Executor for Spinner {
    type Context = support::Guest;

    fn run(&mut self, permit: &RunPermit, guest: &mut support::Guest) -> Stopped {
        guest.steps = guest.steps.saturating_add(1);
        while !self.interrupt.requested(permit) {
            thread::yield_now();
        }
        Stopped::new(Stop::Interrupted, permit.budget())
    }
}

#[test]
fn an_interrupt_reaches_the_running_worker_without_the_lock() {
    let (host, _, made) = support::host(1, 1, 0, clock());
    let guest = made[0];
    thread::scope(|scope| {
        let worker = WorkerId::new(0);
        let mut spinner = Spinner { interrupt: host.interrupt(worker).unwrap() };
        let running = {
            let host = host.clone();
            scope.spawn(move || host.work(worker, &mut spinner))
        };
        // 実行中になるのを待ってから中断を求める。実行器は錠を取らずに求めを見て止まる。
        for round in 1..=3 {
            host.wait_until(|state| state.kernel.running_count() == 1 && state.personality.yields == round - 1).unwrap();
            host.with(|state| state.kernel.preempt_thread(guest)).unwrap().unwrap();
            host.wait_until(|state| state.personality.yields == round).unwrap();
        }
        host.close().unwrap();
        // 閉じても、実行中のゲストは止まるまで続く。もう一度中断を求めて止める。
        host.wait_until(|state| state.kernel.running_count() == 1).unwrap_or(());
        let _ = host.with(|state| state.kernel.preempt_thread(guest));
        assert_eq!(running.join().unwrap(), Ok(()));
    });
    host.with(|state| {
        state.kernel.audit().unwrap();
        assert_eq!(state.kernel.thread_phase(guest), Ok(ThreadPhase::Ready));
    })
    .unwrap();
    assert_eq!(host.interrupt(WorkerId::new(1)).map(|_| ()), Err(HostFault::NoWorker));
}

#[test]
fn an_idle_worker_wakes_for_the_next_deadline() {
    let (host, process, _) = support::host(1, 0, 0, clock());
    let timer = host
        .with(|state| {
            let object = state.kernel.create_object(process, SignalSpec::new(SignalMode::Keep, 0, 1)).unwrap();
            let timer = state.kernel.create_timer(process, object).unwrap();
            let at = Instant::from_nanos(state.kernel.now().nanos().saturating_add(20_000_000));
            state.kernel.arm_timer(timer, at, None).unwrap();
            timer
        })
        .unwrap();
    thread::scope(|scope| {
        let running = {
            let host = host.clone();
            scope.spawn(move || host.work(WorkerId::new(0), &mut support::Stepper))
        };
        // 誰も状態を変えなくても、眠っている worker が期限で起きて時刻を進める。
        host.wait_until(|state| state.kernel.next_deadline().is_none()).unwrap();
        host.close().unwrap();
        assert_eq!(running.join().unwrap(), Ok(()));
    });
    host.with(|state| {
        assert!(state.kernel.now() >= Instant::from_nanos(20_000_000));
        state.kernel.drop_timer(timer).unwrap();
        state.kernel.audit().unwrap();
    })
    .unwrap();
}

#[test]
fn a_failing_worker_closes_the_host_for_the_others() {
    // 文脈の無いスレッドを実行しようとした worker は失敗で戻り、ほかの worker も終わる。
    let (host, _, made) = support::host(2, 1, 0, clock());
    host.with(|state| state.contexts.remove(made[0]).map(|_| ())).unwrap().unwrap();
    thread::scope(|scope| {
        let running: Vec<_> = [0, 1]
            .into_iter()
            .map(|worker| {
                let host = host.clone();
                scope.spawn(move || host.work(WorkerId::new(worker), &mut support::Stepper))
            })
            .collect();
        let results: Vec<_> = running.into_iter().map(|worker| worker.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    });
    host.with(|state| {
        state.kernel.audit().unwrap();
        assert_eq!(state.kernel.thread_phase(made[0]), Ok(ThreadPhase::Servicing));
    })
    .unwrap();
    let _ = ThreadFate::Ready;
}

#[test]
fn the_flat_store_keeps_contents_within_each_backing() {
    let (host, process, _) = support::host(1, 0, 0, clock());
    let (a, b) = host
        .with(|state| (state.kernel.create_backing(process, 8192).unwrap(), state.kernel.create_backing(process, 4096).unwrap()))
        .unwrap();
    let mut store = FlatStore::new();
    assert!(store.is_empty());
    assert_eq!(store.create(a, 8192), Ok(()));
    assert_eq!(store.create(a, 1), Err(fault::DUPLICATE));
    assert_eq!(store.create(b, 4096), Ok(()));
    assert_eq!(store.len(), 2);
    assert_eq!(store.write(a, BackingOffset::new(8190), b"xy"), Ok(()));
    assert_eq!(store.write(a, BackingOffset::new(8191), b"xy"), Err(fault::OUT_OF_BOUNDS));
    assert_eq!(store.write(a, BackingOffset::new(u64::MAX), b"x"), Err(fault::OUT_OF_BOUNDS));
    let mut out = [9u8; 3];
    assert_eq!(store.read(a, BackingOffset::new(8189), &mut out), Ok(()));
    assert_eq!(out, [0, b'x', b'y']);
    assert_eq!(store.read(a, BackingOffset::new(8190), &mut out), Err(fault::OUT_OF_BOUNDS));
    // 実体は互いに独立している。
    assert_eq!(store.read(b, BackingOffset::new(4093), &mut out), Ok(()));
    assert_eq!(out, [0, 0, 0]);
    assert!(store.retire(a) && !store.retire(a));
    assert_eq!(store.read(a, BackingOffset::new(0), &mut out), Err(fault::NO_BACKING));
    assert_eq!(store.write(a, BackingOffset::new(0), b"z"), Err(fault::NO_BACKING));
    assert_eq!(store.create(b, u64::MAX), Err(fault::DUPLICATE));
}
