//! 規則の自己試験: 今のリポジトリに違反が無いこと、壊せば必ず検出すること。

use std::path::Path;

use crate::{RULES, Repo, check, run};

fn repo() -> Repo {
    Repo::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

#[test]
fn the_repository_is_clean() {
    let findings = run(&repo());
    let shown: Vec<String> =
        findings.iter().map(|f| format!("{}:{}: [{}] {}", f.path, f.line, f.rule, f.message)).collect();
    assert!(findings.is_empty(), "違反がある:\n{}", shown.join("\n"));
}

#[test]
fn every_rule_detects_each_of_its_breakages() {
    let clean = repo();
    for rule in RULES {
        assert!(!rule.breaks.is_empty(), "{}: 壊し方が一つも無い規則は、検出できるか分からない", rule.id);
        for (at, breakage) in rule.breaks.iter().enumerate() {
            let mut broken = clean.clone();
            breakage(&mut broken);
            assert_ne!(broken, clean, "{} の壊し方 {at} が何も変えていない (置き換える文字列が無くなった)", rule.id);
            let findings = check(rule, &broken);
            assert!(!findings.is_empty(), "{} が壊し方 {at} を検出しなかった", rule.id);
            // 壊し方が構文を壊しただけなら、規則そのものを確かめたことにならない。
            assert!(
                findings.iter().all(|finding| !finding.message.contains("読めない")),
                "{} の壊し方 {at} は読めないファイルを作っただけ: {findings:?}",
                rule.id
            );
        }
    }
}

#[test]
fn rule_names_are_unique() {
    for (at, rule) in RULES.iter().enumerate() {
        assert!(RULES.iter().skip(at + 1).all(|other| other.id != rule.id), "{} が二つある", rule.id);
    }
}

#[test]
fn test_only_code_is_outside_the_production_rules() {
    let text = "#[cfg(test)]\nmod tests { fn f() -> Vec<u8> { vec![] } }\nfn g() {}\n";
    let production = crate::scan::scan(text, true).unwrap();
    assert!(production.idents.iter().all(|(name, _)| name != "Vec"));
    assert!(production.macros.is_empty());
    let everything = crate::scan::scan(text, false).unwrap();
    assert!(everything.idents.iter().any(|(name, _)| name == "Vec"));
    // 否定つきの cfg は本番のコードである。
    let negated = crate::scan::scan("#[cfg(not(test))]\nfn f() -> Vec<u8> { loop {} }\n", true).unwrap();
    assert!(negated.idents.iter().any(|(name, _)| name == "Vec"));
}

#[test]
fn findings_carry_the_line() {
    let mut broken = repo();
    let before = broken.text("crates/kome/src/kernel.rs").lines().count();
    broken.append("crates/kome/src/kernel.rs", "static COUNT: u32 = 0;");
    let rule = RULES.iter().find(|rule| rule.id == "no-globals").unwrap();
    let findings = check(rule, &broken);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].line, before + 2);
}

/// 必須の試験が全て合格した出力を、リポジトリから組み立てる。
fn passing_log(repo: &Repo) -> String {
    let mut names = std::collections::BTreeSet::new();
    for (_, text) in repo.under("crates/", ".rs") {
        for (name, attrs) in crate::scan::scan(text, false).unwrap().fns {
            if attrs.iter().any(|attr| attr == "test") {
                names.insert(name);
            }
        }
    }
    for line in repo.text("docs/invariants.toml").lines() {
        if let Some((_, name)) = line.trim().trim_end_matches(',').trim_matches('"').rsplit_once("::") {
            names.insert(name.to_owned());
        }
    }
    let mut log: String = names.iter().map(|name| format!("test tests::{name} ... ok\n")).collect();
    for line in repo.text("docs/compile-fail.tsv").lines().filter(|line| !line.starts_with('#')) {
        let (path, count) = line.split_once('\t').unwrap();
        for at in 0..count.parse::<usize>().unwrap() {
            log.push_str(&format!("test {path} - module (line {at}) - compile fail ... ok\n"));
        }
    }
    log.push_str("test result: ok. 9 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s\n");
    log
}

#[test]
fn a_complete_run_is_accepted() {
    let repo = repo();
    assert_eq!(crate::check_run(&repo, &passing_log(&repo)), Vec::<String>::new());
}

#[test]
fn a_run_that_left_out_any_required_test_is_rejected() {
    let repo = repo();
    let log = passing_log(&repo);
    type Breakage = fn(&str) -> String;
    let breakages: [(&str, Breakage); 8] = [
        ("台帳の caught が走っていない", |log| log.replace("test tests::audit_detects_two_permits_for_one_thread ... ok\n", "")),
        ("台帳の試験 (proptest) が走っていない", |log| log.replace("test tests::every_property_holds_for_any_operation_sequence ... ok\n", "")),
        ("台帳に無い #[test] が走っていない", |log| log.replace("test tests::rule_names_are_unique ... ok\n", "")),
        ("無視された", |log| log.replacen("... ok", "... ignored", 1)),
        ("失敗した", |log| log.replacen("... ok", "... FAILED", 1)),
        ("無視の数が 0 でない", |log| log.replace(" 0 ignored;", " 1 ignored;")),
        ("絞り込んで走らせた", |log| log.replace(" 0 filtered out;", " 97 filtered out;")),
        ("compile_fail が一つ走っていない", |log| log.replacen("- compile fail ... ok\n", "... ok\n", 1)),
    ];
    for (what, breakage) in breakages {
        let broken = breakage(&log);
        assert_ne!(broken, log, "{what}: 壊し方が何も変えていない");
        assert!(!crate::check_run(&repo, &broken).is_empty(), "{what}: 検出しなかった");
    }
    assert!(!crate::check_run(&repo, "").is_empty(), "何も走っていない出力を合格にした");
}
