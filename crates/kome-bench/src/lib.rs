//! kome の性能の測定に使う小さな計測器。
//!
//! 依存を増やさないために、統計の道具は持たない。一回あたりの時間が 1 ms 以上になる回数を
//! まとめて測り、その中央値と最小値を出す。数値は機械に依存するので、合否には使わない。
//! `--smoke` では各操作を一度だけ実行し、測定の筋書きが動くことだけを確かめる (CI が使う)。

use std::fmt::Debug;
use std::hint::black_box;
use std::time::{Duration, Instant};

/// 測定の一回ぶんをまとめる時間の下限。
const BATCH: Duration = Duration::from_millis(1);
/// まとめて測る回数。
const SAMPLES: usize = 31;

/// 失敗しないはずの結果を取り出す。失敗は測定の筋書きの誤りなので、その場で止める。
pub fn must<T, E: Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("測定の筋書きが失敗した: {error:?}"),
    }
}

/// 無いはずのない値を取り出す。
pub fn some<T>(value: Option<T>) -> T {
    match value {
        Some(value) => value,
        None => panic!("測定の筋書きが失敗した: 値が無い"),
    }
}

/// 計測器。
#[derive(Debug)]
pub struct Bench {
    smoke: bool,
    filter: Option<String>,
}

impl Bench {
    /// コマンドラインの引数から作る。`--smoke` で一度だけの実行、それ以外の語は名前の絞り込み。
    #[must_use]
    pub fn from_args() -> Bench {
        let mut bench = Bench { smoke: false, filter: None };
        for arg in std::env::args().skip(1) {
            if arg == "--smoke" {
                bench.smoke = true;
            } else if !arg.starts_with('-') {
                bench.filter = Some(arg);
            }
        }
        if !bench.smoke {
            println!("名前\t中央値 (ns)\t最小値 (ns)");
        }
        bench
    }

    /// 一度だけの実行か。
    #[must_use]
    pub const fn smoke(&self) -> bool {
        self.smoke
    }

    fn wanted(&self, name: &str) -> bool {
        self.filter.as_ref().is_none_or(|filter| name.contains(filter.as_str()))
    }

    /// `op` 一回あたりの時間を測る。
    pub fn measure<R>(&self, name: &str, mut op: impl FnMut() -> R) {
        if !self.wanted(name) {
            return;
        }
        if self.smoke {
            black_box(op());
            println!("{name}\tok");
            return;
        }
        let mut time = |rounds: u64| {
            let start = Instant::now();
            for _ in 0..rounds {
                black_box(op());
            }
            start.elapsed()
        };
        let mut rounds = 1u64;
        while time(rounds) < BATCH && rounds < (1 << 30) {
            rounds = rounds.saturating_mul(2);
        }
        let mut samples: Vec<f64> = (0..SAMPLES).map(|_| per_op(time(rounds), rounds)).collect();
        report(name, &mut samples);
    }

    /// まとまった仕事 (`ops` 回ぶんの操作を含む) を何度か実行し、操作一回あたりの時間を出す。
    /// `ops` は `--smoke` のときの回数と、それ以外のときの回数から `work` が選ぶ。
    pub fn measure_whole(&self, name: &str, ops: u64, mut work: impl FnMut(u64)) {
        if !self.wanted(name) {
            return;
        }
        if self.smoke {
            work(1);
            println!("{name}\tok");
            return;
        }
        let mut samples: Vec<f64> = (0..5)
            .map(|_| {
                let start = Instant::now();
                work(ops);
                per_op(start.elapsed(), ops)
            })
            .collect();
        report(name, &mut samples);
    }
}

fn per_op(elapsed: Duration, rounds: u64) -> f64 {
    // 表示のための割り算。桁が落ちても測定の意味は変わらない。
    elapsed.as_secs_f64() * 1e9 / f64::from(u32::try_from(rounds).unwrap_or(u32::MAX))
}

fn report(name: &str, samples: &mut [f64]) {
    samples.sort_by(f64::total_cmp);
    let median = samples.get(samples.len() / 2).copied().unwrap_or(f64::NAN);
    let least = samples.first().copied().unwrap_or(f64::NAN);
    println!("{name}\t{median:.1}\t{least:.1}");
}
