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
