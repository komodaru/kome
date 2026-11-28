//! 必須の試験が実際に走って合格したことを、`cargo test` の出力と照合する。
//!
//! 必須の試験は、リポジトリにある全ての `#[test]`、台帳 (`docs/invariants.toml`) が名指しする
//! 試験、`docs/compile-fail.tsv` が数える compile_fail 試験である。走らなかった試験
//! (無視、絞り込み、対象から外れたクレート) があれば、合格として数えない。

use std::collections::BTreeSet;

use crate::Repo;
use crate::scan::scan;

/// Loom と Shuttle の検査の置き場。
const LOOM_TESTS: &str = "crates/kome-host-std/tests/loom.rs";
const SHUTTLE_TESTS: &str = "crates/kome-host-std/tests/shuttle.rs";

/// 出力の一行を (試験の名前, 結果) に分ける。
fn outcome(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("test ")?;
    let (name, result) = rest.rsplit_once(" ... ")?;
    Some((name, result.trim()))
}

/// 必須の試験の名前 (関数名)。
fn required(repo: &Repo, problems: &mut Vec<String>) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for (path, text) in repo.under("crates/", ".rs") {
        // Loom と Shuttle の検査は別の設定 (`--cfg loom`、`--cfg shuttle`) でしか組み立てられない。
        // 走ったことは scripts/ci/loom.sh と scripts/ci/shuttle.sh が照合する。
        if path == LOOM_TESTS || path == SHUTTLE_TESTS {
            continue;
        }
        match scan(text, false) {
            Ok(found) => {
                for (name, attrs) in found.fns {
                    if attrs.iter().any(|attr| attr == "test") {
                        names.insert(name);
                    }
                }
            }
            Err(error) => problems.push(format!("{path}: 構文を読めない: {error}")),
        }
    }
    match repo.text("docs/invariants.toml").parse::<toml::Table>() {
        Ok(ledger) => {
            let entries = ledger.get("invariant").and_then(toml::Value::as_array).map_or(&[][..], Vec::as_slice);
            for entry in entries {
                let caught = entry.get("caught").and_then(toml::Value::as_str);
                let tests = entry.get("tests").and_then(toml::Value::as_array).map_or(&[][..], Vec::as_slice);
                for test in tests.iter().filter_map(toml::Value::as_str).chain(caught) {
                    match test.rsplit_once("::") {
                        Some((_, name)) => {
                            names.insert(name.to_owned());
                        }
                        None => problems.push(format!("台帳の試験 {test} は「ファイル::名前」と書く")),
                    }
                }
            }
        }
        Err(error) => problems.push(format!("docs/invariants.toml を読めない: {error}")),
    }
    names
}

/// `log` (`cargo test --workspace` の出力) を必須の試験と照合し、問題を並べる。空なら合格。
pub fn check_run(repo: &Repo, log: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let names = required(repo, &mut problems);
    if names.is_empty() {
        problems.push("必須の試験が一つも見つからない".to_owned());
    }
    let mut passed = BTreeSet::new();
    for line in log.lines() {
        if let Some((name, result)) = outcome(line) {
            if result == "ok" {
                let last = name.rsplit("::").next().unwrap_or(name);
                passed.insert(last.split(' ').next().unwrap_or(last));
            } else {
                problems.push(format!("合格していない試験: {line}"));
            }
        }
        if line.starts_with("test result:") {
            let clean = line.contains(" 0 failed;") && line.contains(" 0 ignored;") && line.contains(" 0 filtered out;");
            if !clean {
                problems.push(format!("失敗・無視・絞り込みがある: {line}"));
            }
        }
    }
    for name in &names {
        if !passed.contains(name.as_str()) {
            problems.push(format!("必須の試験 {name} が走っていない"));
        }
    }
    let listed = repo.text("docs/compile-fail.tsv").lines().filter(|line| !line.starts_with('#') && !line.is_empty());
    for (path, count) in listed.filter_map(|line| line.split_once('\t')) {
        let want: usize = count.parse().unwrap_or(usize::MAX);
        let ran = log
            .lines()
            .filter_map(outcome)
            .filter(|(name, result)| name.starts_with(&format!("{path} - ")) && name.ends_with("- compile fail") && *result == "ok")
            .count();
        if ran != want {
            problems.push(format!("{path}: compile_fail 試験は {want} 個のはずが、{ran} 個しか走っていない"));
        }
    }
    problems
}
