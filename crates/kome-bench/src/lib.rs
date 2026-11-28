//! kome の性能の測定に使う小さな計測器。
//!
//! 依存を増やさないために、統計の道具は持たない。一回あたりの時間が 1 ms 以上になる回数を
//! まとめて測り、その中央値と最小値を出す。
//!
//! - `--smoke` では各操作を一度だけ実行し、測定の筋書きが動くことだけを確かめる。
//! - `--gate ファイル` では、遅くなっていないかを判定する。時間そのものは機械に依存するので、
//!   同じ実行の中で測った基準の仕事 ([`reference_work`]) との比を、記録してある比と比べる。
//!   記録の [`LIMIT`] 倍を超えた操作があれば失敗する。
//! - `--record ファイル` は、測った最小値 (ns) をファイルへ足し込む。ファイルに既にある値とは、
//!   操作ごとに小さい方を残す。共有の機械では、ほかの仕事に邪魔された測定は遅くなるだけなので、
//!   何度か測った中の最小値が、邪魔されなかったときの時間に最も近い。
//! - `--against ファイル` は、同じ機械で測った相手 (一つ前のコミットを `--record` したもの) と比べる。
//!   幅は [`AGAINST_LIMIT`] 倍。相手に無い操作 (新しく足した測定) は比べない。
//! - `--baseline ファイル` は、今の比を記録として書き出す。
//!
//! - `--counts ファイル` は、時間ではなく数 (命令、確保、キャッシュの外れ、分岐の予測の外れ) を
//!   操作ごとに数え、記録と比べる。`--counts-baseline ファイル` は記録を書き出す ([`counts`])。
//!
//! 二つの判定の使い分けは `scripts/ci/bench-gate.sh` と docs/bench.md にある。

pub mod counts;

use std::cell::RefCell;
use std::fmt::Debug;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// 測定の一回ぶんをまとめる時間の下限。
const BATCH: Duration = Duration::from_millis(1);
/// まとめて測る回数。
const SAMPLES: usize = 31;
/// 記録してある比の何倍までを、遅くなっていないと見なすか。
///
/// 共有の機械での揺れと、機械による比の違い (メモリを多く触る操作と基準の仕事の速さの比は
/// CPU によって変わる) を吸収する幅。計算量が変わる退行 (全てを見て選ぶ、など) は桁で変わるので、
/// この幅でも見つかる。
pub const LIMIT: f64 = 2.0;
/// 同じ機械で交互に測った、一つ前のコミットの比の何倍までを、遅くなっていないと見なすか。
/// 機械の違いが入らないので、[`LIMIT`] より狭い。邪魔されなかった測定どうしの揺れ (数 %) より広く取る。
pub const AGAINST_LIMIT: f64 = 1.15;
/// 基準の仕事の名前。記録の比は、これの最小値で割ったもの。
pub const REFERENCE: &str = "基準の仕事";

/// 基準の仕事: メモリに触れない決まった量の計算。機械の速さを測るためだけにある。
#[must_use]
pub fn reference_work(seed: u64) -> u64 {
    let mut state = seed | 1;
    for _ in 0..64 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
    }
    state
}

/// 判定の結果の一行。
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// 記録の幅に収まった。今の比と記録の比。
    Within(f64, f64),
    /// 記録の幅を超えた。今の比と記録の比。
    Slower(f64, f64),
    /// 記録にある操作を測っていない (名前が変わった、測定を外した)。
    NotMeasured,
}

/// 記録 (`名前<TAB>比` の行) と、測った最小値 (ns) を比べる。`reference` は基準の仕事の最小値。
///
/// 記録の比の `limit` 倍までを収まったと見なす。記録の全ての行に結果を返す。記録を読めなければ、その理由を返す。
pub fn judge(baseline: &str, measured: &[(String, f64)], reference: f64, limit: f64) -> Result<Vec<(String, Verdict)>, String> {
    if !reference.is_finite() || reference <= 0.0 {
        return Err(format!("{REFERENCE} の時間 {reference} が正でない"));
    }
    let mut verdicts = Vec::new();
    for line in baseline.lines().filter(|line| !line.is_empty() && !line.starts_with('#')) {
        let (name, recorded) = line.split_once('\t').ok_or(format!("記録の行を読めない: {line}"))?;
        let recorded: f64 = recorded.trim().parse().map_err(|_| format!("記録の比を読めない: {line}"))?;
        if !recorded.is_finite() || recorded <= 0.0 {
            return Err(format!("記録の比が正でない: {line}"));
        }
        let verdict = match measured.iter().find(|(measured, _)| measured == name) {
            None => Verdict::NotMeasured,
            Some((_, least)) => {
                let ratio = least / reference;
                // 測れなかった値 (NaN) は、収まったとは見なさない。
                if ratio <= recorded * limit { Verdict::Within(ratio, recorded) } else { Verdict::Slower(ratio, recorded) }
            }
        };
        verdicts.push((name.to_owned(), verdict));
    }
    if verdicts.is_empty() {
        return Err("記録に操作が一つも無い".into());
    }
    Ok(verdicts)
}

/// `cargo bench` は crate のディレクトリで動く。引数のパスは、リポジトリの根から読む。
fn from_root(path: &str) -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../..")).join(path)
}

/// `--record` のファイル (`名前<TAB>最小値 (ns)` の行。基準の仕事の行を含む) を読む。
pub fn parse_record(text: &str) -> Result<Vec<(String, f64)>, String> {
    let mut rows = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let (name, least) = line.split_once('\t').ok_or(format!("測定の行を読めない: {line}"))?;
        let least: f64 = least.trim().parse().map_err(|_| format!("測定の値を読めない: {line}"))?;
        rows.push((name.to_owned(), least));
    }
    Ok(rows)
}

/// 二つの測定を、操作ごとに小さい方を残して一つにする。片方にしか無い操作は、そのまま残す。
#[must_use]
pub fn merge(earlier: &[(String, f64)], now: &[(String, f64)]) -> Vec<(String, f64)> {
    let mut merged = earlier.to_vec();
    for (name, least) in now {
        match merged.iter_mut().find(|(known, _)| known == name) {
            // 測れなかった値 (NaN) は、小さい方として残さない。
            Some((_, kept)) => *kept = if *least < *kept || kept.is_nan() { *least } else { *kept },
            None => merged.push((name.clone(), *least)),
        }
    }
    merged
}

/// 測定 (基準の仕事の行を含む) を、判定に使う記録の形 (`名前<TAB>比` の行) にする。
pub fn ratios(rows: &[(String, f64)]) -> Result<String, String> {
    let reference = rows.iter().find(|(name, _)| name == REFERENCE).ok_or(format!("{REFERENCE} の行が無い"))?.1;
    if !reference.is_finite() || reference <= 0.0 {
        return Err(format!("{REFERENCE} の時間 {reference} が正でない"));
    }
    Ok(rows.iter().filter(|(name, _)| name != REFERENCE).map(|(name, least)| format!("{name}\t{:.6}\n", least / reference)).collect())
}

/// 測った結果をどう使うか。
#[derive(Debug)]
enum Use {
    /// 表示するだけ。
    Show,
    /// 記録と比べて判定する。
    Gate(PathBuf),
    /// 同じ機械で直前に測った比と比べて判定する。
    Against(PathBuf),
    /// 記録として書き出す。
    Baseline(PathBuf),
    /// 一つの操作を決まった回数だけ行い、その間の確保の数を出す (Cachegrind の上で動かすための形)。
    CountOne(String, u64),
    /// 操作ごとに数を数え、記録と比べて判定する。
    Counts(PathBuf),
    /// 操作ごとに数を数え、記録として書き出す。
    CountsBaseline(PathBuf),
}

/// 確保を数えるアロケータの口。測定の実行ファイルが、大域のアロケータと一緒に渡す。
#[derive(Clone, Copy, Debug)]
pub struct Allocations {
    /// 数え始める。
    pub enter: fn(),
    /// 数えるのをやめる。
    pub leave: fn(),
    /// 印の間に起きた確保の数。
    pub count: fn() -> u64,
}

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
    usage: Use,
    /// 測った最小値を足し込むファイル。
    record: Option<PathBuf>,
    /// 測った操作の名前と最小値 (ns)。
    least: RefCell<Vec<(String, f64)>>,
    allocations: Allocations,
    /// 数で測る操作の名前 (`--counts` のとき)。
    names: RefCell<Vec<String>>,
}

impl Bench {
    /// コマンドラインの引数から作る。`--smoke` で一度だけの実行、`--gate ファイル` で判定、
    /// `--baseline ファイル` で記録の書き出し、それ以外の語は名前の絞り込み。
    #[must_use]
    pub fn from_args(allocations: Allocations) -> Bench {
        let mut bench = Bench {
            smoke: false,
            filter: None,
            usage: Use::Show,
            record: None,
            least: RefCell::new(Vec::new()),
            allocations,
            names: RefCell::new(Vec::new()),
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            if arg == "--smoke" {
                bench.smoke = true;
            } else if arg == "--gate" {
                bench.usage = Use::Gate(from_root(&some(args.next())));
            } else if arg == "--record" {
                bench.record = Some(from_root(&some(args.next())));
            } else if arg == "--against" {
                bench.usage = Use::Against(from_root(&some(args.next())));
            } else if arg == "--baseline" {
                bench.usage = Use::Baseline(from_root(&some(args.next())));
            } else if arg == "--count-one" {
                let name = some(args.next());
                bench.usage = Use::CountOne(name, must(some(args.next()).parse()));
            } else if arg == "--counts" {
                bench.usage = Use::Counts(from_root(&some(args.next())));
            } else if arg == "--counts-baseline" {
                bench.usage = Use::CountsBaseline(from_root(&some(args.next())));
            } else if !arg.starts_with('-') {
                bench.filter = Some(arg);
            }
        }
        if !bench.smoke && bench.timed() {
            println!("名前\t中央値 (ns)\t最小値 (ns)");
        }
        bench.measure(REFERENCE, {
            let mut seed = 1u64;
            move || {
                seed = reference_work(seed);
                seed
            }
        });
        bench
    }

    /// 全ての測定の後に呼ぶ。`--gate` と `--against` なら判定し、遅くなった操作があれば理由を返す。
    /// 記録へ書き出すときは、参照ホストを複数の worker で動かす測定 (錠の奪い合いで揺れる) を除く。
    pub fn finish(self) -> Result<(), String> {
        let mut least = self.least.into_inner();
        least.retain(|(name, _)| !name.contains("worker 4 個"));
        if let Some(path) = &self.record {
            // 無いファイルは、まだ何も測っていないということ。読めない中身は失敗にする。
            let earlier = match std::fs::read_to_string(path) {
                Ok(text) => parse_record(&text)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(error) => return Err(format!("{}: {error}", path.display())),
            };
            least = merge(&earlier, &least);
            let text: String = least.iter().map(|(name, least)| format!("{name}\t{least:.3}\n")).collect();
            std::fs::write(path, text).map_err(|error| format!("{}: {error}", path.display()))?;
        }
        let reference = least.iter().find(|(name, _)| name == REFERENCE).map_or(f64::NAN, |(_, least)| *least);
        match self.usage {
            Use::Show | Use::CountOne(..) => Ok(()),
            Use::Counts(path) => {
                let recorded = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
                let exe = std::env::current_exe().map_err(|error| format!("自分の実行ファイルが分からない: {error}"))?;
                let measured = counts::count_all(&exe, &self.names.into_inner())?;
                Bench::decide_counts(&counts::parse(&recorded)?, &measured)
            }
            Use::CountsBaseline(path) => {
                let exe = std::env::current_exe().map_err(|error| format!("自分の実行ファイルが分からない: {error}"))?;
                let measured = counts::count_all(&exe, &self.names.into_inner())?;
                let mut text = String::from(
                    "# 操作一回あたりの数: 命令、確保、一次データキャッシュの外れ、分岐の予測の外れ (docs/bench.md)。\n# `cargo bench -p kome-bench -- --counts-baseline docs/bench-counts.tsv` が書く。\n",
                );
                text.extend(measured.iter().map(counts::line));
                std::fs::write(&path, text).map_err(|error| format!("{}: {error}", path.display()))
            }
            Use::Baseline(path) => {
                let mut text = format!("# 操作の最小値を「{REFERENCE}」の最小値で割った比。`cargo bench -p kome-bench -- --baseline docs/bench-gate.tsv` が書く。\n");
                for (name, least) in least.iter().filter(|(name, _)| name != REFERENCE) {
                    text.push_str(&format!("{name}\t{:.3}\n", least / reference));
                }
                std::fs::write(&path, text).map_err(|error| format!("{}: {error}", path.display()))
            }
            Use::Gate(path) => {
                let baseline = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
                Bench::decide(&baseline, &least, reference, LIMIT, true)
            }
            Use::Against(path) => {
                let other = std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
                let theirs = ratios(&parse_record(&other)?)?;
                if theirs.is_empty() {
                    println!("\n相手に、比べる操作が無い");
                    return Ok(());
                }
                Bench::decide(&theirs, &least, reference, AGAINST_LIMIT, false)
            }
        }
    }

    /// 記録と比べて判定し、結果を表示する。`all` なら、記録にある操作を測っていないことも失敗にする。
    fn decide(baseline: &str, least: &[(String, f64)], reference: f64, limit: f64, all: bool) -> Result<(), String> {
        let verdicts = judge(baseline, least, reference, limit)?;
        let mut failed = 0u32;
        println!("\n名前\t今の比\t記録の比\t判定 (記録の {limit} 倍まで)");
        for (name, verdict) in verdicts.iter().filter(|(_, verdict)| all || *verdict != Verdict::NotMeasured) {
            match verdict {
                Verdict::Within(ratio, recorded) => println!("{name}\t{ratio:.3}\t{recorded:.3}\tok"),
                Verdict::Slower(ratio, recorded) => {
                    failed = failed.saturating_add(1);
                    println!("{name}\t{ratio:.3}\t{recorded:.3}\t遅くなった");
                }
                Verdict::NotMeasured => {
                    failed = failed.saturating_add(1);
                    println!("{name}\t-\t-\t測っていない");
                }
            }
        }
        if failed == 0 { Ok(()) } else { Err(format!("{failed} 個の操作が、記録の {limit} 倍に収まらなかった")) }
    }

    /// 数の記録と比べて判定し、結果を表示する。
    fn decide_counts(recorded: &[counts::Counts], measured: &[counts::Counts]) -> Result<(), String> {
        let limit = counts::COUNT_LIMIT;
        let mut failed = 0u32;
        println!("名前\t命令\t確保\tキャッシュの外れ\t予測の外れ\t判定 (命令は記録の {limit} 倍まで、確保は増えないこと)");
        for (name, verdict) in counts::judge(recorded, measured, limit) {
            let shown = measured.iter().find(|counts| counts.name == name).map_or_else(|| format!("{name}\t-\t-\t-\t-"), |now| counts::line(now).trim_end().to_owned());
            let word = match verdict {
                counts::Counted::Within(_, recorded) => format!("ok (記録は {recorded:.1})"),
                counts::Counted::MoreWork(_, recorded) => format!("命令が増えた (記録は {recorded:.1})"),
                counts::Counted::MoreAllocations(_, recorded) => format!("確保が増えた (記録は {recorded:.3})"),
                counts::Counted::NotMeasured => "測っていない".to_owned(),
                counts::Counted::NotRecorded => "記録に無い".to_owned(),
            };
            if !word.starts_with("ok") {
                failed = failed.saturating_add(1);
            }
            println!("{shown}\t{word}");
        }
        if failed == 0 { Ok(()) } else { Err(format!("{failed} 個の操作が、数の記録に収まらなかった")) }
    }

    /// 時間を測る実行か (数を数える実行ではないか)。
    const fn timed(&self) -> bool {
        matches!(self.usage, Use::Show | Use::Gate(_) | Use::Against(_) | Use::Baseline(_))
    }

    /// 数を数える実行なら、`op` を数える側の流儀で扱い、真を返す。
    fn counted<R>(&self, name: &str, op: &mut impl FnMut() -> R) -> bool {
        match &self.usage {
            Use::CountOne(wanted, rounds) => {
                if wanted == name {
                    (self.allocations.enter)();
                    for _ in 0..*rounds {
                        black_box(op());
                    }
                    (self.allocations.leave)();
                    println!("allocs\t{}", (self.allocations.count)());
                }
                true
            }
            Use::Counts(_) | Use::CountsBaseline(_) => {
                self.names.borrow_mut().push(name.to_owned());
                true
            }
            Use::Show | Use::Gate(_) | Use::Against(_) | Use::Baseline(_) => false,
        }
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
        if !self.wanted(name) || self.counted(name, &mut op) {
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
        self.least.borrow_mut().push((name.to_owned(), report(name, &mut samples)));
    }

    /// まとまった仕事 (`ops` 回ぶんの操作を含む) を何度か実行し、操作一回あたりの時間を出す。
    /// `ops` は `--smoke` のときの回数と、それ以外のときの回数から `work` が選ぶ。
    ///
    /// 数では測らない (別のスレッドで動く仕事は、数える印の外にある)。
    pub fn measure_whole(&self, name: &str, ops: u64, mut work: impl FnMut(u64)) {
        if !self.wanted(name) || !self.timed() {
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
        self.least.borrow_mut().push((name.to_owned(), report(name, &mut samples)));
    }
}

fn per_op(elapsed: Duration, rounds: u64) -> f64 {
    // 表示のための割り算。桁が落ちても測定の意味は変わらない。
    elapsed.as_secs_f64() * 1e9 / f64::from(u32::try_from(rounds).unwrap_or(u32::MAX))
}

/// 中央値と最小値を表示し、最小値を返す。
fn report(name: &str, samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    let median = samples.get(samples.len() / 2).copied().unwrap_or(f64::NAN);
    let least = samples.first().copied().unwrap_or(f64::NAN);
    println!("{name}\t{median:.1}\t{least:.1}");
    least
}

#[cfg(test)]
mod tests {
    use super::{AGAINST_LIMIT, LIMIT, REFERENCE, Verdict, judge, merge, parse_record, ratios};

    fn measured(pairs: &[(&str, f64)]) -> Vec<(String, f64)> {
        pairs.iter().map(|(name, least)| ((*name).to_owned(), *least)).collect()
    }

    #[test]
    fn the_gate_compares_ratios_not_times() {
        let baseline = "# 注釈\n借りる\t2.000\n引く\t0.500\n";
        // 機械が 10 倍遅くても、基準の仕事も同じだけ遅ければ比は変わらない。
        for speed in [1.0, 10.0] {
            let seen = measured(&[("借りる", 200.0 * speed), ("引く", 50.0 * speed)]);
            let verdicts = judge(baseline, &seen, 100.0 * speed, LIMIT).unwrap();
            assert_eq!(verdicts, vec![("借りる".to_owned(), Verdict::Within(2.0, 2.0)), ("引く".to_owned(), Verdict::Within(0.5, 0.5))]);
        }
    }

    #[test]
    fn the_gate_fails_an_operation_past_the_limit_and_one_that_was_not_measured() {
        let baseline = "借りる\t2.000\n引く\t0.500\n";
        let edge = 2.0 * LIMIT * 100.0;
        let verdicts = judge(baseline, &measured(&[("借りる", edge)]), 100.0, LIMIT).unwrap();
        assert_eq!(verdicts, vec![("借りる".to_owned(), Verdict::Within(2.0 * LIMIT, 2.0)), ("引く".to_owned(), Verdict::NotMeasured)]);
        let verdicts = judge(baseline, &measured(&[("借りる", edge + 1.0), ("引く", f64::NAN)]), 100.0, LIMIT).unwrap();
        assert!(matches!(verdicts.as_slice(), [(_, Verdict::Slower(..)), (_, Verdict::Slower(..))]), "{verdicts:?}");
    }

    #[test]
    fn the_comparison_with_the_previous_commit_is_narrower() {
        // 記録との比較では収まる 2 割の悪化が、一つ前のコミットとの比較では遅くなったと判定される。
        let baseline = "借りる\t1.000\n";
        let seen = measured(&[("借りる", 120.0)]);
        assert_eq!(judge(baseline, &seen, 100.0, LIMIT).unwrap(), vec![("借りる".to_owned(), Verdict::Within(1.2, 1.0))]);
        assert_eq!(judge(baseline, &seen, 100.0, AGAINST_LIMIT).unwrap(), vec![("借りる".to_owned(), Verdict::Slower(1.2, 1.0))]);
        let seen = measured(&[("借りる", 110.0)]);
        assert_eq!(judge(baseline, &seen, 100.0, AGAINST_LIMIT).unwrap(), vec![("借りる".to_owned(), Verdict::Within(1.1, 1.0))]);
    }

    #[test]
    fn repeated_measurements_keep_the_least_of_each_operation() {
        // 邪魔された測定は遅くなるだけなので、何度か測った中の最小値を残す。
        let first = measured(&[(REFERENCE, 100.0), ("借りる", 90.0), ("引く", f64::NAN)]);
        let second = measured(&[(REFERENCE, 140.0), ("借りる", 60.0), ("引く", 20.0), ("送る", 70.0)]);
        let merged = merge(&first, &second);
        assert_eq!(merged, measured(&[(REFERENCE, 100.0), ("借りる", 60.0), ("引く", 20.0), ("送る", 70.0)]));
        assert_eq!(merge(&merged, &first)[..3], merged[..3]);
        // 書き出した形を読み戻せる。比は、基準の仕事の最小値で割ったもの。
        let text: String = merged.iter().map(|(name, least)| format!("{name}\t{least:.3}\n")).collect();
        assert_eq!(parse_record(&text).unwrap(), merged);
        assert_eq!(ratios(&merged).unwrap(), "借りる\t0.600000\n引く\t0.200000\n送る\t0.700000\n");
        assert!(ratios(&merged[1..]).is_err(), "基準の仕事の無い測定から比を作った");
        assert!(ratios(&measured(&[(REFERENCE, 0.0), ("借りる", 1.0)])).is_err());
        assert!(parse_record("借りる 1.0\n").is_err() && parse_record("借りる\tx\n").is_err());
    }

    #[test]
    fn the_gate_refuses_a_record_it_cannot_read() {
        let seen = measured(&[("借りる", 1.0)]);
        for (baseline, reference) in [("", 1.0), ("# 注釈だけ\n", 1.0), ("借りる 2.0\n", 1.0), ("借りる\tx\n", 1.0), ("借りる\t0\n", 1.0), ("借りる\t-1\n", 1.0), ("借りる\t2.0\n", 0.0), ("借りる\t2.0\n", f64::NAN)] {
            assert!(judge(baseline, &seen, reference, LIMIT).is_err(), "{baseline:?} {reference}");
        }
    }
}
