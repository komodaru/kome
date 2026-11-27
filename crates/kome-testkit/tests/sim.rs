//! 決定的シミュレーションの property test。
//!
//! 失敗したバイト列は proptest が縮めて `tests/sim.proptest-regressions` に残す。
//! それを消さずにコミットすると、以後は毎回最初に再生される。
#![cfg(test)]

use std::collections::BTreeSet;

use kome::Effect;
use kome_testkit::drive;
use proptest::prelude::*;

/// 試験用の決定的な乱数列 (xorshift64*)。
fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes()[7]
        })
        .collect()
}

fn name(effect: &Effect) -> &'static str {
    match effect {
        Effect::Interrupt { .. } => "Interrupt",
        Effect::WaitDone { .. } => "WaitDone",
        Effect::ThreadExited { .. } => "ThreadExited",
        Effect::ProcessExited { .. } => "ProcessExited",
        Effect::ProcessReclaimed { .. } => "ProcessReclaimed",
        Effect::ObjectReclaimed { .. } => "ObjectReclaimed",
        Effect::BackingReclaimed { .. } => "BackingReclaimed",
        Effect::IoSubmit { .. } => "IoSubmit",
        Effect::IoCancel { .. } => "IoCancel",
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 3000, ..ProptestConfig::default() })]

    #[test]
    fn every_property_holds_for_any_operation_sequence(bytes in proptest::collection::vec(any::<u8>(), 0..700)) {
        if let Err(failure) = drive(&bytes) {
            prop_assert!(false, "操作 {}: {}", failure.step, failure.message);
        }
    }

    #[test]
    fn the_same_inputs_give_the_same_state_and_effects(bytes in proptest::collection::vec(any::<u8>(), 0..400)) {
        let first = drive(&bytes);
        let second = drive(&bytes);
        prop_assert!(first.is_ok(), "{first:?}");
        prop_assert_eq!(first, second);
    }
}

#[test]
fn long_runs_hold_every_property() {
    for seed in 0..300 {
        if let Err(failure) = drive(&noise(seed, 6000)) {
            panic!("seed {seed} 操作 {}: {}", failure.step, failure.message);
        }
    }
}

/// 生成する操作列が空回りしていないこと: すべての種類の `Effect` が出て、操作の多くが受理される。
#[test]
fn the_generator_reaches_every_effect() {
    let mut seen = BTreeSet::new();
    let (mut accepted, mut refused) = (0usize, 0usize);
    for seed in 0..200 {
        let summary = drive(&noise(seed, 3000)).unwrap();
        seen.extend(summary.effects.iter().map(name));
        accepted += summary.accepted;
        refused += summary.refused;
    }
    let all = [
        "Interrupt",
        "WaitDone",
        "ThreadExited",
        "ProcessExited",
        "ProcessReclaimed",
        "ObjectReclaimed",
        "BackingReclaimed",
        "IoSubmit",
        "IoCancel",
    ];
    assert_eq!(seen, BTreeSet::from(all));
    assert!(accepted * 2 > refused, "受理 {accepted}、拒否 {refused}: 操作列が拒否ばかりになっている");
}

#[test]
fn a_failure_reports_the_step() {
    // 空の入力は何もしない。
    let summary = drive(&[]).unwrap();
    assert_eq!((summary.steps, summary.accepted, summary.refused), (0, 0, 0));
    // 何も無い状態での操作は選ぶ相手が無く、受理も拒否もされない。
    let summary = drive(&[3, 0]).unwrap();
    assert_eq!((summary.steps, summary.accepted), (1, 0));
}

/// 保存してある入力 (fuzz や property test が見つけたもの、種) を毎回再生する。
#[test]
fn saved_inputs_still_pass() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/regressions");
    let mut replayed = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let bytes = std::fs::read(&path).unwrap();
        if let Err(failure) = drive(&bytes) {
            panic!("{}: 操作 {}: {}", path.display(), failure.step, failure.message);
        }
        replayed += 1;
    }
    assert!(replayed > 0, "{dir} に入力が無い");
}
