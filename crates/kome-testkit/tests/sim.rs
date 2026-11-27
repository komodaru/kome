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
    let (mut accepted, mut refused, mut restores, mut after_restore) = (0usize, 0usize, 0u64, 0usize);
    for seed in 0..200 {
        let summary = drive(&noise(seed, 3000)).unwrap();
        seen.extend(summary.effects.iter().map(name));
        accepted += summary.accepted;
        refused += summary.refused;
        restores += summary.restores;
        if summary.restores > 0 {
            // 復元した状態が動き続けていること: 乗り換えた後の状態も資源を持っている。
            after_restore += usize::try_from(summary.kernel.census().threads()).unwrap();
            assert_eq!(summary.kernel.instance().get(), 99 + summary.restores);
        }
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
    assert!(restores > 200 && after_restore > 0, "復元 {restores} 回: 写しからの復元にほとんど到達していない");
    assert!(accepted * 2 > refused, "受理 {accepted}、拒否 {refused}: 操作列が拒否ばかりになっている");
}

/// 通信路の操作が空回りしていないこと: 送信と受信のすべての種類の結果、資源の受け渡し、
/// 中身ごとの破棄、回収に到達する。
#[test]
fn the_generator_reaches_every_channel_outcome() {
    let mut reached = BTreeSet::new();
    for seed in 0..200 {
        reached.extend(drive(&noise(seed, 3000)).unwrap().reached.into_iter().filter(|tag| !tag.contains(':')));
    }
    let all = [
        "sent",
        "sent-part",
        "sent-object",
        "sent-backing",
        "full",
        "no-receiver",
        "too-long",
        "received",
        "received-object",
        "received-backing",
        "empty",
        "ended",
        "too-small",
        "dropped-parcel",
        "reclaimed",
    ];
    assert_eq!(reached, BTreeSet::from(all));
}

/// 実行権の貸し借りが空回りしていないこと: 止まる理由の全て、状況と合わない理由の拒否、
/// 優先度が待ち順を追い越す貸し出し、名指しの貸し出し、譲らせる要求に到達する。
#[test]
fn the_generator_reaches_every_scheduling_outcome() {
    let mut reached = BTreeSet::new();
    for seed in 0..200 {
        reached.extend(drive(&noise(seed, 3000)).unwrap().reached.into_iter().filter(|tag| tag.starts_with("run:")));
    }
    let all = [
        "run:trap",
        "run:yielded",
        "run:budget",
        "run:interrupted",
        "run:fault",
        "run:exited",
        "run:budget-remains",
        "run:not-interrupted",
        "run:outranked",
        "run:named",
        "run:preempted",
    ];
    assert_eq!(reached, BTreeSet::from(all));
}

/// プロセスへの割り当てが空回りしていないこと: 三つの割り当てのそれぞれで、超える操作の拒否に到達する。
/// 拒否と受理が割り当てどおりであることは、操作のたびに確かめている。
#[test]
fn the_generator_reaches_every_quota() {
    let mut reached = BTreeSet::new();
    for seed in 0..200 {
        reached.extend(drive(&noise(seed, 3000)).unwrap().reached.into_iter().filter(|tag| tag.starts_with("quota:")));
    }
    assert_eq!(reached, BTreeSet::from(["quota:threads", "quota:ios", "quota:mapped"]));
}

/// ゲストのアドレスでの読み書きが空回りしていないこと: 書き込み、読み出し、ページの境界をまたぐ
/// 読み出し、先頭での失敗、範囲の途中での失敗に到達する。
#[test]
fn the_generator_reaches_every_guest_access() {
    let mut reached = BTreeSet::new();
    for seed in 0..200 {
        reached.extend(drive(&noise(seed, 3000)).unwrap().reached.into_iter().filter(|tag| tag.starts_with("mem:")));
    }
    assert_eq!(reached, BTreeSet::from(["mem:written", "mem:read", "mem:read-across", "mem:fault", "mem:fault-inside"]));
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
