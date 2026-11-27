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
    explore(2, 1, 1, Some(5));
}

#[test]
fn two_workers_run_two_threads_to_their_ends() {
    explore(2, 2, 0, Some(5));
}
