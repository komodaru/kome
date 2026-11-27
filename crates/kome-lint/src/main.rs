//! `cargo run -q -p kome-lint` で、リポジトリ全体を規則に照らして確かめる。

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
