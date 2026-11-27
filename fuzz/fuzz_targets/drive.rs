//! バイト列を操作列として kome に適用し、決定的シミュレーションの全ての性質を確かめる。
//!
//! property test (crates/kome-testkit/tests/sim.rs) と同じ `drive` を使う。見つけた入力は
//! crates/kome-testkit/tests/regressions/ へ置くと、以後は通常の試験で毎回再生される。
#![no_main]

libfuzzer_sys::fuzz_target!(|bytes: &[u8]| {
    if let Err(failure) = kome_testkit::drive(bytes) {
        panic!("操作 {}: {}", failure.step, failure.message);
    }
});
