//! 時間ではなく、数で測る: 操作一回あたりの命令の数、確保の数、キャッシュの外れ、分岐の予測の外れ。
//!
//! 時間は、機械と、同じ機械で動くほかの仕事に左右される。数は左右されない。同じ実行ファイルを
//! Valgrind (Cachegrind) の上で動かすと、実行した命令の数は毎回同じになる。確保の数は、確保を
//! 数えるアロケータで数える。
//!
//! 操作一回あたりの数は、操作を `ROUNDS` 回行う実行と `2 × ROUNDS` 回行う実行の差を `ROUNDS` で
//! 割って出す (準備にかかる分が消える)。キャッシュの大きさは固定して与えるので、キャッシュの外れの
//! 数は、動かした機械のキャッシュの大きさに左右されない。

use std::process::Command;

/// 短い方の実行で操作を行う回数。
pub const ROUNDS: u64 = 1000;
/// 記録した命令の数の何倍までを、増えていないと見なすか。
///
/// 命令の数は同じ実行ファイルなら変わらないが、C ライブラリの版や、Valgrind が見せる CPU の機能で、
/// コピーなどの実装が変わることがある。その違いを吸収する幅。
pub const COUNT_LIMIT: f64 = 1.05;

/// 一回の実行から読んだ数。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Totals {
    /// 実行した命令の数。
    pub instructions: u64,
    /// 一次データキャッシュの外れの数。
    pub data_misses: u64,
    /// 分岐の予測の外れの数。
    pub mispredicts: u64,
    /// 操作を行っている間の確保の数。
    pub allocations: u64,
}

/// 操作一回あたりの数。
#[derive(Clone, Debug, PartialEq)]
pub struct Counts {
    /// 操作の名前。
    pub name: String,
    /// 実行した命令の数。
    pub instructions: f64,
    /// 確保の数。
    pub allocations: f64,
    /// 一次データキャッシュの外れの数。
    pub data_misses: f64,
    /// 分岐の予測の外れの数。
    pub mispredicts: f64,
}

fn real(count: u64) -> f64 {
    // 表示と比較のための変換。差は 32 ビットに収まる大きさしか扱わない。
    f64::from(u32::try_from(count).unwrap_or(u32::MAX))
}

/// Cachegrind のまとめの行 (`==番号== 名前: 数 …`) から、名前の数を読む。
fn summary(report: &str, key: &str) -> Result<u64, String> {
    for line in report.lines() {
        let Some((_, rest)) = line.split_once("== ") else { continue };
        let Some((name, value)) = rest.split_once(':') else { continue };
        if name.split_whitespace().collect::<Vec<_>>().join(" ") != key {
            continue;
        }
        let digits: String = value.split_whitespace().next().unwrap_or("").chars().filter(|c| *c != ',').collect();
        return digits.parse().map_err(|_| format!("Cachegrind の行を読めない: {line}"));
    }
    Err(format!("Cachegrind の出力に「{key}」の行が無い"))
}

/// 一回の実行の出力から数を読む。`report` は Cachegrind のまとめ (標準エラー)、`out` は測定の
/// 実行ファイルの標準出力 (`allocs<TAB>数` の行を含む)。
pub fn totals(report: &str, out: &str) -> Result<Totals, String> {
    let allocations = out.lines().find_map(|line| line.strip_prefix("allocs\t")).ok_or("測定の出力に確保の数が無い (名前が合う操作が無い)")?;
    Ok(Totals {
        instructions: summary(report, "I refs")?,
        data_misses: summary(report, "D1 misses")?,
        mispredicts: summary(report, "Mispredicts")?,
        allocations: allocations.trim().parse().map_err(|_| format!("確保の数を読めない: {allocations}"))?,
    })
}

/// `rounds` 回の実行と、その二倍の回数の実行から、操作一回あたりの数を出す。
pub fn per_op(name: &str, once: Totals, twice: Totals, rounds: u64) -> Result<Counts, String> {
    let each = |short: u64, long: u64| -> Result<f64, String> {
        let more = long.checked_sub(short).ok_or(format!("{name}: 回数を倍にした実行の方が、数が少ない ({long} < {short})"))?;
        Ok(real(more) / real(rounds))
    };
    if rounds == 0 {
        return Err("回数が 0 では割れない".into());
    }
    Ok(Counts {
        name: name.to_owned(),
        instructions: each(once.instructions, twice.instructions)?,
        allocations: each(once.allocations, twice.allocations)?,
        // キャッシュと分岐の予測は、スタックの置き場所で数個だけ揺れる。操作がほとんど外さないとき、
        // 倍の回数の方が少なく出ることがあるので、そのときは 0 とする (判定には使わない数である)。
        data_misses: real(twice.data_misses.saturating_sub(once.data_misses)) / real(rounds),
        mispredicts: real(twice.mispredicts.saturating_sub(once.mispredicts)) / real(rounds),
    })
}

/// 記録の一行。
#[must_use]
pub fn line(counts: &Counts) -> String {
    let Counts { name, instructions, allocations, data_misses, mispredicts } = counts;
    format!("{name}\t{instructions:.1}\t{allocations:.3}\t{data_misses:.3}\t{mispredicts:.3}\n")
}

/// 記録 (`名前<TAB>命令<TAB>確保<TAB>キャッシュの外れ<TAB>予測の外れ` の行) を読む。
pub fn parse(text: &str) -> Result<Vec<Counts>, String> {
    let mut rows = Vec::new();
    for line in text.lines().filter(|line| !line.is_empty() && !line.starts_with('#')) {
        let cells: Vec<&str> = line.split('\t').collect();
        let [name, instructions, allocations, data_misses, mispredicts] = cells.as_slice() else {
            return Err(format!("記録の行を読めない: {line}"));
        };
        let number = |cell: &str| -> Result<f64, String> {
            let value: f64 = cell.trim().parse().map_err(|_| format!("記録の数を読めない: {line}"))?;
            if value.is_finite() && value >= 0.0 { Ok(value) } else { Err(format!("記録の数が 0 以上でない: {line}")) }
        };
        rows.push(Counts {
            name: (*name).to_owned(),
            instructions: number(instructions)?,
            allocations: number(allocations)?,
            data_misses: number(data_misses)?,
            mispredicts: number(mispredicts)?,
        });
    }
    if rows.is_empty() {
        return Err("記録に操作が一つも無い".into());
    }
    Ok(rows)
}

/// 判定の結果の一行。
#[derive(Clone, Debug, PartialEq)]
pub enum Counted {
    /// 命令の数が幅に収まり、確保の数は増えていない。今の命令の数と、記録の命令の数。
    Within(f64, f64),
    /// 命令の数が幅を超えた。今の数と、記録の数。
    MoreWork(f64, f64),
    /// 確保の数が増えた。今の数と、記録の数。
    MoreAllocations(f64, f64),
    /// 記録にある操作を測っていない。
    NotMeasured,
    /// 測った操作が記録に無い (新しく足した測定は、同じコミットで記録する)。
    NotRecorded,
}

/// 記録と、測った数を比べる。記録の全ての行と、記録に無い測定の全てに、結果を返す。
///
/// 確保の数は、少しでも増えたら増えたと判定する (数は実行ごとに変わらないので、幅は要らない)。
/// 命令の数は、記録の `limit` 倍までを収まったと見なす。
#[must_use]
pub fn judge(recorded: &[Counts], measured: &[Counts], limit: f64) -> Vec<(String, Counted)> {
    let mut verdicts = Vec::new();
    for record in recorded {
        let verdict = match measured.iter().find(|counts| counts.name == record.name) {
            None => Counted::NotMeasured,
            // 記録は小数第三位までなので、その丸めの分だけは増えたと見なさない。
            Some(now) if now.allocations > record.allocations + 0.0005 => Counted::MoreAllocations(now.allocations, record.allocations),
            Some(now) if now.instructions <= record.instructions * limit => Counted::Within(now.instructions, record.instructions),
            Some(now) => Counted::MoreWork(now.instructions, record.instructions),
        };
        verdicts.push((record.name.clone(), verdict));
    }
    for now in measured.iter().filter(|now| !recorded.iter().any(|record| record.name == now.name)) {
        verdicts.push((now.name.clone(), Counted::NotRecorded));
    }
    verdicts
}

/// 測定の実行ファイル `exe` を Cachegrind の上で動かし、操作 `name` を `rounds` 回行わせて、数を読む。
pub fn observe(exe: &std::path::Path, name: &str, rounds: u64) -> Result<Totals, String> {
    let mut valgrind = Command::new("valgrind");
    // キャッシュの大きさを固定する (一次 32 KiB・8 連想、最終 8 MiB・16 連想、行は 64 バイト)。
    valgrind.args(["--tool=cachegrind", "--cache-sim=yes", "--branch-sim=yes", "--I1=32768,8,64", "--D1=32768,8,64", "--LL=8388608,16,64"]);
    valgrind.arg("--cachegrind-out-file=/dev/null").arg(exe).args(["--count-one", name, &rounds.to_string()]);
    let output = valgrind.output().map_err(|error| format!("valgrind を動かせない: {error}"))?;
    let report = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!("{name}: Cachegrind の上の実行が失敗した\n{report}"));
    }
    totals(&report, &String::from_utf8_lossy(&output.stdout))
}

/// 操作を一つずつ Cachegrind の上で動かし、一回あたりの数を出す。操作どうしは並べて動かす
/// (数は、同時に動くほかの仕事に左右されない)。
pub fn count_all(exe: &std::path::Path, names: &[String]) -> Result<Vec<Counts>, String> {
    let lanes = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let mut counted = Vec::new();
    for group in names.chunks(lanes) {
        let results: Vec<Result<Counts, String>> = std::thread::scope(|scope| {
            let running: Vec<_> = group
                .iter()
                .map(|name| {
                    scope.spawn(move || {
                        let once = observe(exe, name, ROUNDS)?;
                        let twice = observe(exe, name, ROUNDS.saturating_mul(2))?;
                        per_op(name, once, twice, ROUNDS)
                    })
                })
                .collect();
            running.into_iter().map(|lane| lane.join().unwrap_or_else(|_| Err("数える途中で止まった".into()))).collect()
        });
        for result in results {
            counted.push(result?);
        }
    }
    Ok(counted)
}

#[cfg(test)]
mod tests {
    use super::{COUNT_LIMIT, Counted, Counts, Totals, judge, line, parse, per_op, totals};

    const REPORT: &str = "==77== Cachegrind, a high-precision tracing profiler\n==77== \n==77== I refs:        1,965,087\n==77== I1  misses:        1,512\n==77== D refs:          612,345  (401,000 rd   + 211,345 wr)\n==77== D1  misses:        4,412  (      3,404 rd   +       1,008 wr)\n==77== LL misses:         3,718  (      2,842 rd   +         876 wr)\n==77== Branches:        300,000  (290,000 cond + 10,000 ind)\n==77== Mispredicts:       8,573  (      8,297 cond +         276 ind)\n";

    fn counts(name: &str, instructions: f64, allocations: f64) -> Counts {
        Counts { name: name.to_owned(), instructions, allocations, data_misses: 0.5, mispredicts: 0.25 }
    }

    #[test]
    fn the_summary_of_cachegrind_is_read_by_name() {
        let read = totals(REPORT, "名前\tok\nallocs\t12\n").unwrap();
        assert_eq!(read, Totals { instructions: 1_965_087, data_misses: 4412, mispredicts: 8573, allocations: 12 });
        // 行が欠けた出力、確保の数が無い出力、読めない数は、0 として通さない。
        assert!(totals(&REPORT.replace("I refs", "J refs"), "allocs\t1\n").is_err());
        assert!(totals(&REPORT.replace("Mispredicts", "Predicts"), "allocs\t1\n").is_err());
        assert!(totals(&REPORT.replace("4,412", "many"), "allocs\t1\n").is_err());
        assert!(totals(REPORT, "名前\tok\n").is_err() && totals(REPORT, "allocs\tx\n").is_err());
    }

    #[test]
    fn the_count_of_one_operation_is_the_difference_of_two_runs() {
        // 準備に 1000 命令、操作一回に 42 命令かかるなら、差を回数で割ると 42 が出る。
        let run = |rounds: u64| Totals { instructions: 1000 + 42 * rounds, data_misses: 7 + rounds / 4, mispredicts: 300 - rounds / 100, allocations: 2 * rounds };
        let each = per_op("借りる", run(100), run(200), 100).unwrap();
        assert_eq!(each, Counts { name: "借りる".to_owned(), instructions: 42.0, allocations: 2.0, data_misses: 0.25, mispredicts: 0.0 });
        assert!(per_op("借りる", run(200), run(100), 100).is_err(), "倍の回数の方が少ない数を通した");
        assert!(per_op("借りる", run(100), run(200), 0).is_err());
    }

    #[test]
    fn a_record_is_written_and_read_back() {
        let rows = vec![counts("借りる", 151.0, 0.0), counts("写す", 20_480.5, 3.0)];
        let text: String = std::iter::once("# 注釈\n".to_owned()).chain(rows.iter().map(line)).collect();
        assert_eq!(parse(&text).unwrap(), rows);
        for broken in ["", "# 注釈だけ\n", "借りる\t1\t0\t0\n", "借りる\tx\t0\t0\t0\n", "借りる\t-1\t0\t0\t0\n", "借りる\tNaN\t0\t0\t0\n"] {
            assert!(parse(broken).is_err(), "{broken:?}");
        }
    }

    #[test]
    fn more_instructions_past_the_limit_fail_and_any_more_allocations_fail() {
        let recorded = vec![counts("借りる", 100.0, 0.0), counts("写す", 1000.0, 2.0), counts("外す", 50.0, 1.0)];
        let edge = 100.0 * COUNT_LIMIT;
        let seen = vec![counts("借りる", edge, 0.0), counts("写す", 900.0, 2.001), counts("送る", 10.0, 0.0)];
        assert_eq!(
            judge(&recorded, &seen, COUNT_LIMIT),
            vec![
                ("借りる".to_owned(), Counted::Within(edge, 100.0)),
                ("写す".to_owned(), Counted::MoreAllocations(2.001, 2.0)),
                ("外す".to_owned(), Counted::NotMeasured),
                ("送る".to_owned(), Counted::NotRecorded),
            ]
        );
        let seen = vec![counts("借りる", edge + 0.5, 0.0), counts("写す", 900.0, 1.0), counts("外す", f64::NAN, 1.0)];
        let verdicts = judge(&recorded, &seen, COUNT_LIMIT);
        // 減った確保と減った命令は通る。読めなかった数 (NaN) は、収まったとは見なさない。
        assert!(matches!(verdicts.as_slice(), [(_, Counted::MoreWork(..)), (_, Counted::Within(..)), (_, Counted::MoreWork(..))]), "{verdicts:?}");
    }
}
