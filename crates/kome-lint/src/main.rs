//! `cargo run -q -p kome-lint` で、リポジトリ全体を規則に照らして確かめる。
//!
//! `cargo run -q -p kome-lint -- ran <cargo test の出力>` は、必須の試験が全て走って合格したことを確かめる。

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let repo = match kome_lint::Repo::load(&root) {
        Ok(repo) => repo,
        Err(error) => {
            eprintln!("リポジトリを読めない: {error}");
            return ExitCode::FAILURE;
        }
    };
    if std::env::args().any(|arg| arg == "--list") {
        for rule in kome_lint::RULES {
            println!("{:<20} {}", rule.id, rule.about);
        }
        return ExitCode::SUCCESS;
    }
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "dup") {
        // 見つけた写しの組を、一覧に載っているものも含めて全て出す。
        let known = kome_lint::dup::known(repo.text(kome_lint::dup::KNOWN)).unwrap_or_default();
        for group in kome_lint::dup::find(repo.all()) {
            let mark = if known.iter().any(|entry| kome_lint::dup::listed(&group, entry)) { "一覧にある" } else { "一覧に無い" };
            println!("{} {} ({mark})", group.kind.id(), group.size);
            for member in &group.members {
                println!("    {}:{}-{} {}", member.file, member.first, member.last, member.name);
            }
            println!("    {}", kome_lint::dup::entry_line(&group));
        }
        return ExitCode::SUCCESS;
    }
    if args.get(1).is_some_and(|arg| arg == "cover") {
        let Some(Ok(lcov)) = args.get(2).map(std::fs::read_to_string) else {
            eprintln!("使い方: kome-lint cover <lcov の記録>");
            return ExitCode::FAILURE;
        };
        let problems = kome_lint::check_cover(&repo, &lcov);
        for problem in &problems {
            eprintln!("{problem}");
        }
        if problems.is_empty() {
            println!("kome-lint cover: kome の全ての行を、試験か一覧の検証が確かめている");
            return ExitCode::SUCCESS;
        }
        eprintln!("kome-lint cover: {} 件の問題", problems.len());
        return ExitCode::FAILURE;
    }
    if args.get(1).is_some_and(|arg| arg == "ran") {
        let log = match args.get(2).map(std::fs::read_to_string) {
            Some(Ok(log)) => log,
            Some(Err(error)) => {
                eprintln!("試験の出力を読めない: {error}");
                return ExitCode::FAILURE;
            }
            None => {
                eprintln!("使い方: kome-lint ran <cargo test の出力>");
                return ExitCode::FAILURE;
            }
        };
        let problems = kome_lint::check_run(&repo, &log);
        for problem in &problems {
            eprintln!("{problem}");
        }
        if problems.is_empty() {
            println!("kome-lint ran: 必須の試験は全て走って合格した");
            return ExitCode::SUCCESS;
        }
        eprintln!("kome-lint ran: {} 件の問題", problems.len());
        return ExitCode::FAILURE;
    }
    let findings = kome_lint::run(&repo);
    for finding in &findings {
        eprintln!("{}:{}: [{}] {}", finding.path, finding.line, finding.rule, finding.message);
    }
    if findings.is_empty() {
        println!("kome-lint: {} 個の規則、違反なし", kome_lint::RULES.len());
        ExitCode::SUCCESS
    } else {
        eprintln!("kome-lint: {} 件の違反", findings.len());
        ExitCode::FAILURE
    }
}
