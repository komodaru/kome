//! kome の設計上の規則を、後戻りしないように機械で確かめる。
//!
//! 規則は [`RULES`] に並ぶ。どの規則も、実際のリポジトリを一か所だけ壊す操作 (`breaks`) を
//! 持ち、試験はその壊れ方を規則が検出することを確かめる (`tests.rs`)。検出できない規則は
//! 規則として数えない。
//!
//! 型や API で防げる誤りはそちらで防ぐ。ここにあるのは、型では表せない約束である:
//! 依存を持たないこと、OS の語彙を持ち込まないこと、台帳と実装と検証が食い違わないこと。

mod ran;
mod rules;
mod scan;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::Path;

pub use ran::check_run;
pub use rules::RULES;

/// リポジトリの中の、文字として読めるファイル。パスは `/` 区切りの相対パス。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Repo {
    files: BTreeMap<String, String>,
}

/// 読まないディレクトリ (生成物と外から取ってきたもの)。
const SKIPPED_DIRS: [&str; 5] = [".git", "target", "states", "corpus", "artifacts"];

impl Repo {
    /// `root` 以下を読む。
    pub fn load(root: &Path) -> std::io::Result<Repo> {
        let mut repo = Repo::default();
        repo.walk(root, "")?;
        Ok(repo)
    }

    fn walk(&mut self, dir: &Path, prefix: &str) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            if entry.file_type()?.is_dir() {
                if !SKIPPED_DIRS.contains(&name.as_str()) {
                    self.walk(&entry.path(), &path)?;
                }
            } else if let Ok(text) = std::fs::read_to_string(entry.path()) {
                self.files.insert(path, text);
            }
        }
        Ok(())
    }

    /// ファイルの内容。無ければ空。
    pub fn text(&self, path: &str) -> &str {
        self.files.get(path).map_or("", String::as_str)
    }

    /// ファイルがあるか。
    pub fn has(&self, path: &str) -> bool {
        self.files.contains_key(path)
    }

    /// `prefix` で始まり `suffix` で終わるパスのファイル。
    pub fn under<'a>(&'a self, prefix: &'a str, suffix: &'a str) -> impl Iterator<Item = (&'a str, &'a str)> {
        self.files
            .iter()
            .filter(move |(path, _)| path.starts_with(prefix) && path.ends_with(suffix))
            .map(|(path, text)| (path.as_str(), text.as_str()))
    }

    /// ファイルを書き換える (無ければ作る)。
    pub fn edit(&mut self, path: &str, change: impl FnOnce(&str) -> String) {
        let changed = change(self.text(path));
        self.files.insert(path.to_owned(), changed);
    }

    /// ファイルの末尾へ足す。
    pub fn append(&mut self, path: &str, more: &str) {
        self.edit(path, |text| format!("{text}\n{more}\n"));
    }

    /// ファイルを消す。
    pub fn remove(&mut self, path: &str) {
        self.files.remove(path);
    }
}

/// 見つけた違反。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// 規則の名前。
    pub rule: &'static str,
    /// ファイル。
    pub path: String,
    /// 行 (1 始まり)。
    pub line: usize,
    /// 何が違反で、どう直すか。
    pub message: String,
}

/// 一つの規則が違反を書き込む先。
#[derive(Debug)]
pub struct Report {
    rule: &'static str,
    findings: Vec<Finding>,
}

impl Report {
    /// 違反を一つ記録する。
    pub fn add(&mut self, path: &str, line: usize, message: impl Into<String>) {
        self.findings.push(Finding { rule: self.rule, path: path.to_owned(), line, message: message.into() });
    }

    /// `needle` を含む最初の行で違反を記録する (無ければ 1 行目)。
    pub fn add_at(&mut self, repo: &Repo, path: &str, needle: &str, message: impl Into<String>) {
        let line = repo.text(path).lines().position(|line| line.contains(needle)).map_or(1, |at| at.saturating_add(1));
        self.add(path, line, message);
    }
}

/// 一つの規則。
pub struct Rule {
    /// 名前。
    pub id: &'static str,
    /// 何を守るか。
    pub about: &'static str,
    /// 検査。
    pub check: fn(&Repo, &mut Report),
    /// 規則が検出しなければならない壊し方。一つ以上。
    pub breaks: &'static [fn(&mut Repo)],
}

/// 一つの規則を適用する。
pub fn check(rule: &Rule, repo: &Repo) -> Vec<Finding> {
    let mut report = Report { rule: rule.id, findings: Vec::new() };
    (rule.check)(repo, &mut report);
    report.findings
}

/// すべての規則を適用する。
pub fn run(repo: &Repo) -> Vec<Finding> {
    RULES.iter().flat_map(|rule| check(rule, repo)).collect()
}
