//! 解放・終了・起床などの「確保しない」と約束した操作 (docs/memory.md) が、決定的
//! シミュレーションのどの状態から呼んでも確保しないことを確かめる。

use kome_noalloc::{Counting, count, enter, leave};
use kome_testkit::{Probe, drive_probed, noise};

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const PROBE: Probe = Probe { enter, leave };

#[test]
fn the_counter_sees_an_allocation_between_the_marks() {
    let before = count();
    enter();
    let boxed = Box::new(7u64);
    leave();
    assert_eq!(*boxed, 7);
    assert_eq!(count(), before + 1, "印の間の確保を数えていない");
    let grown = {
        let mut list = vec![1u8];
        enter();
        list.reserve(4096);
        leave();
        list.capacity()
    };
    assert!(grown >= 4096);
    assert_eq!(count(), before + 2, "再確保を数えていない");
    let outside = Box::new(1u8);
    assert_eq!(*outside, 1);
    assert_eq!(count(), before + 2, "印の外の確保を数えた");
}

#[test]
fn promised_operations_never_allocate() {
    let mut steps = 0;
    let mut accepted = 0;
    for seed in 0..300 {
        let before = count();
        let summary = match drive_probed(&noise(seed, 6000), PROBE) {
            Ok(summary) => summary,
            Err(failure) => panic!("seed {seed} 操作 {}: {}", failure.step, failure.message),
        };
        assert_eq!(count(), before, "seed {seed}: 確保しないと約束した操作が確保した");
        steps += summary.steps;
        accepted += summary.accepted;
    }
    assert!(steps > 100_000 && accepted > 30_000, "操作 {steps}、受理 {accepted}: ほとんど動いていない");
}

#[test]
fn saved_inputs_never_allocate() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../crates/kome-testkit/tests/regressions");
    let mut replayed = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
        let before = count();
        assert!(drive_probed(&bytes, PROBE).is_ok());
        assert_eq!(count(), before, "確保しないと約束した操作が確保した");
        replayed += 1;
    }
    assert!(replayed > 0);
}
