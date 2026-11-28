//! kome の全ての行を、試験のどれかが通ったかを確かめる (`kome-lint cover <lcov>`)。
//!
//! `scripts/ci/cover.sh` が、全ての試験を走らせて行ごとの通過回数を測る。一度も通らなかった行は、
//! 次のどちらかでなければ違反である。
//!
//! - 不変条件が破れたときの返事 (`Error::Broken`) の行。通らないことが正しい。通る入力があれば、
//!   操作のたびに [`Kernel::audit`] を呼ぶ決定的シミュレーションが失敗する。不変条件そのものは台帳
//!   (`docs/invariants.toml`) が検証と結ぶ。
//! - `docs/uncovered.tsv` に、代わりに何が確かめているかを添えて載せた行。
//!
//! 一覧は自分では増えない。載っていない行も、載っているのに通るようになった行も違反である。
//! 試験だけのコード (`tests.rs` などと、ファイルの末尾の `#[cfg(test)] mod`) は数えない。

use std::collections::{BTreeMap, BTreeSet};

use crate::Repo;

/// 通らなくてよい行の一覧。
pub const UNCOVERED: &str = "docs/uncovered.tsv";

/// 測る対象。
const SOURCES: &str = "crates/kome/src/";

/// 試験と証明だけが入るファイル。
const TRIAL_FILES: [&str; 3] = ["tests.rs", "conform.rs", "proofs.rs"];

/// 不変条件が破れたときの返事の書き方。
const BROKEN: [&str; 2] = ["Error::Broken(", "(BROKEN)"];

/// 不変条件が破れたときの返事の行か。
#[must_use]
pub fn replies_broken(code: &str) -> bool {
    BROKEN.iter().any(|reply| code.contains(reply))
}

/// 一覧の一行: ファイル、行の内容 (前後の空白を除く)、代わりに確かめているもの。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Excused {
    /// 一覧の中の行 (1 始まり)。
    pub line: usize,
    /// ファイル。
    pub path: String,
    /// 行の内容。
    pub code: String,
    /// 代わりに確かめているもの。
    pub why: String,
}

/// 一覧を読む。行は `ファイル<TAB>行の内容<TAB># 代わりに確かめているもの`。`#` で始まる行と空行は注釈。
pub fn excused(text: &str) -> Result<Vec<Excused>, (usize, String)> {
    let mut out = Vec::new();
    for (at, line) in text.lines().enumerate() {
        let number = at.saturating_add(1);
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let cells: Vec<&str> = line.split('\t').collect();
        let [path, code, why] = cells.as_slice() else {
            return Err((number, "`ファイル<TAB>行の内容<TAB># 代わりに確かめているもの` と書く".to_owned()));
        };
        let why = why.strip_prefix("# ").map(str::trim).unwrap_or("");
        if why.chars().count() < 12 {
            return Err((number, "代わりに何が確かめているかが書かれていない (`# …`)".to_owned()));
        }
        out.push(Excused { line: number, path: (*path).to_owned(), code: code.trim().to_owned(), why: why.to_owned() });
    }
    Ok(out)
}

/// lcov の記録から、ファイルごとの (行, 通った回数) を取り出す。対象の外のファイルは捨てる。
fn measured(lcov: &str) -> BTreeMap<String, BTreeMap<usize, u64>> {
    let mut out: BTreeMap<String, BTreeMap<usize, u64>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in lcov.lines() {
        if let Some(file) = line.strip_prefix("SF:") {
            current = file.find(SOURCES).and_then(|at| file.get(at..)).map(str::to_owned);
            if let Some(path) = &current {
                out.entry(path.clone()).or_default();
            }
        } else if let (Some(path), Some(data)) = (&current, line.strip_prefix("DA:")) {
            let mut cells = data.split(',');
            let (Some(number), Some(count)) = (cells.next().and_then(|n| n.parse::<usize>().ok()), cells.next().and_then(|n| n.parse::<u64>().ok())) else {
                continue;
            };
            // 同じ行が複数の実行ファイルに現れたら、通った回数を足す。
            let seen = out.entry(path.clone()).or_default().entry(number).or_insert(0);
            *seen = seen.saturating_add(count);
        }
    }
    out
}

/// 試験だけのコードが始まる行 (1 始まり)。無ければ、ファイルの終わりの次。
fn trial_from(text: &str) -> usize {
    let lines: Vec<&str> = text.lines().collect();
    lines
        .iter()
        .zip(lines.iter().skip(1))
        .position(|(mark, item)| mark.trim() == "#[cfg(test)]" && item.starts_with("mod "))
        .map_or(lines.len().saturating_add(1), |at| at.saturating_add(1))
}

/// 測った結果 `lcov` を調べ、問題を一つずつ文にして返す。空なら合格。
#[must_use]
pub fn check_cover(repo: &Repo, lcov: &str) -> Vec<String> {
    let list = match excused(repo.text(UNCOVERED)) {
        Ok(list) => list,
        Err((line, why)) => return vec![format!("{UNCOVERED}:{line}: {why}")],
    };
    let seen = measured(lcov);
    let mut problems = Vec::new();
    let mut used: BTreeSet<usize> = BTreeSet::new();
    let mut files = 0usize;
    for (path, text) in repo.under(SOURCES, ".rs") {
        if TRIAL_FILES.iter().any(|name| path.strip_prefix(SOURCES) == Some(name)) {
            continue;
        }
        files = files.saturating_add(1);
        let until = trial_from(text);
        let has_code = text.lines().take(until.saturating_sub(1)).any(|line| line.trim_start().starts_with("fn ") || line.contains(" fn "));
        let Some(lines) = seen.get(path) else {
            if has_code {
                problems.push(format!("{path}:1: このファイルが測定に入っていない"));
            }
            continue;
        };
        if has_code && lines.keys().all(|number| *number >= until) {
            problems.push(format!("{path}:1: このファイルの行が一つも測られていない"));
        }
        for (number, count) in lines {
            if *count != 0 || *number >= until {
                continue;
            }
            let code = text.lines().nth(number.saturating_sub(1)).unwrap_or("").trim();
            if replies_broken(code) {
                continue;
            }
            let entry = list.iter().find(|entry| entry.path == path && entry.code == code);
            match entry {
                Some(entry) => {
                    used.insert(entry.line);
                }
                None => problems.push(format!(
                    "{path}:{number}: どの試験も通らない行: `{code}`。通る試験を足す。試験では通せない行なら、代わりに確かめているものを {UNCOVERED} に `{path}\t{code}\t# …` と書く"
                )),
            }
        }
    }
    if files == 0 {
        problems.push(format!("{SOURCES} にファイルが無い"));
    }
    for entry in &list {
        if !used.contains(&entry.line) {
            problems.push(format!("{UNCOVERED}:{}: 一覧にあるが、通らない行ではなくなった (`{}`)。行を消す", entry.line, entry.code));
        }
    }
    problems
}
