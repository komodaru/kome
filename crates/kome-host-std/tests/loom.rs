//! worker の進み方を Loom で試す。
//!
//! `scripts/ci/loom.sh`
#![cfg(loom)]

#[cfg(test)]
mod support;

use kome::{Instant, WorkerId};
use loom::thread;

/// `workers` 個の worker が、`threads` 本のスレッドを終了まで実行する。試した進み方の全てで、
/// 行き詰まらず (取りこぼした通知は Loom が行き詰まりとして検出する)、どのスレッドも決まった
/// 回数だけ実行して終了し、検査を通る。
///
/// `preemptions` は、スレッドが自分からは止まらない場所で切り替える回数の上限。`None` なら
/// 全ての進み方を試す。
fn explore(workers: u32, threads: usize, yields: u64, preemptions: Option<usize>) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = preemptions;
    builder.check(move || {
        let (host, _, made) = support::host(workers, threads, yields, || Instant::from_nanos(0));
        let running: Vec<_> = (0..workers)
            .map(|worker| {
                let host = host.clone();
                thread::spawn(move || host.work(WorkerId::new(worker), &mut support::Stepper))
            })
            .collect();
        host.wait_until(|state| state.personality.process_exited).unwrap();
        host.close().unwrap();
        for worker in running {
            assert_eq!(worker.join().unwrap(), Ok(()));
        }
        support::check(&host, &made, yields);
    });
}

#[test]
fn one_worker_and_the_waiter_never_miss_each_other() {
    explore(1, 1, 1, None);
}

#[test]
fn two_workers_share_one_thread() {
    explore(2, 1, 1, Some(7));
}

#[test]
fn two_workers_run_two_threads_to_their_ends() {
    explore(2, 2, 0, Some(7));
}

/// 眠っている worker が、外からの操作でできた仕事のために起こされる。
#[test]
fn an_idle_worker_is_woken_for_new_work() {
    loom::model(|| {
        let (host, _, made) = support::host(1, 1, 0, || Instant::from_nanos(0));
        let guest = made[0];
        host.with(|state| state.kernel.suspend_thread(guest)).unwrap().unwrap();
        let running = {
            let host = host.clone();
            thread::spawn(move || host.work(WorkerId::new(0), &mut support::Stepper))
        };
        host.with(|state| state.kernel.unsuspend_thread(guest)).unwrap().unwrap();
        host.wait_until(|state| state.personality.process_exited).unwrap();
        host.close().unwrap();
        assert_eq!(running.join().unwrap(), Ok(()));
        support::check(&host, &made, 0);
    });
}

/// 仕事が最初から終わっているホストでは、誰も閉じない。どの進み方でも、worker は眠らずに戻る
/// (眠れば、起こす者がいないので Loom が行き詰まりとして検出する)。
#[test]
fn a_host_finished_from_the_start_lets_its_workers_go() {
    loom::model(|| {
        let host = support::finished_host(2, || Instant::from_nanos(0));
        let running: Vec<_> = (0..2)
            .map(|worker| {
                let host = host.clone();
                thread::spawn(move || host.work(WorkerId::new(worker), &mut support::Stepper))
            })
            .collect();
        for worker in running {
            assert_eq!(worker.join().unwrap(), Ok(()));
        }
    });
}

/// 一本目のスレッドへの中断の求めは、同じ worker が次に実行する二本目を止めない。
/// 求めを立てる側と見る側の進み方を全て試す。
#[test]
fn an_interrupt_for_one_run_never_stops_the_next_thread_on_the_same_worker() {
    loom::model(|| {
        support::interrupt_names_one_run(|| Instant::from_nanos(0), thread::yield_now, |work| {
            let running = thread::spawn(work);
            Box::new(move || running.join().unwrap())
        });
    });
}
