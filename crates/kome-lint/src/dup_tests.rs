//! 写しの検出 ([`crate::dup`]) の試験。種類ごとに、その種類だけが見つけること、見つけてはいけない
//! ものを見つけないこと、一覧の読み方を確かめる。

use crate::dup::{Kind, TRIAL_RUN, entry_line, find, known, listed};

/// 写しの元にする関数。字句の数は、どの種類の最小の大きさよりも多い。
const BODY: &str = "{
    let mut total = 0u64;
    for entry in first.iter().chain(second.iter()) {
        if *entry > 16 && *entry < 4096 {
            total = total.saturating_add(entry.saturating_mul(3)).saturating_sub(1);
        }
    }
    if total > 65536 {
        total = total.saturating_sub(first.len().try_into().unwrap_or(u64::MAX));
    }
    total
}
";

fn function(name: &str, body: &str) -> String {
    format!("fn {name}(first: &[u64], second: &[u64]) -> u64 {body}")
}

/// 見つかった組の種類と、写しの名前。
fn found(files: &[(&str, &str)]) -> Vec<(Kind, Vec<String>)> {
    find(files.iter().copied()).into_iter().map(|group| (group.kind, group.members.into_iter().map(|member| member.name).collect())).collect()
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn a_copied_function_is_found_once_as_exact() {
    let text = format!("{}{}", function("one", BODY), function("two", BODY));
    assert_eq!(found(&[("a.rs", &text)]), vec![(Kind::Exact, names(&["one", "two"]))]);
    // ファイルをまたいでも、メソッドでも同じ。
    let method = format!("struct S;\nimpl S {{\n{}}}\n", function("two", BODY));
    let one = function("one", BODY);
    assert_eq!(found(&[("a.rs", &one), ("b.rs", &method)]), vec![(Kind::Exact, names(&["one", "S::two"]))]);
    // 文書の注釈だけが違っても、同じコードである。
    let documented = format!("/// 一つめ。\n{}/// 二つめの、別の説明。\n{}", function("one", BODY), function("two", BODY));
    assert_eq!(found(&[("a.rs", &documented)]), vec![(Kind::Exact, names(&["one", "two"]))]);
}

#[test]
fn a_copy_with_other_local_names_is_found_as_renamed() {
    // 局所の名前をどう付け替えても、同じ写しである。
    for (total, entry) in [("sum", "each"), ("a", "b"), ("entry", "total"), ("x1", "x2")] {
        let other = BODY.replace("total", "\u{1}").replace("entry", entry).replace('\u{1}', total);
        let text = format!("{}{}", function("one", BODY), function("two", &other));
        assert_eq!(found(&[("a.rs", &text)]), vec![(Kind::Renamed, names(&["one", "two"]))], "{total} {entry}");
    }
    // 呼ぶメソッドや定数が違えば、別のコードである。
    for (from, to) in [("saturating_mul(3)", "saturating_mul(5)"), ("saturating_sub(1)", "wrapping_sub(1)"), ("> 16", ">= 16")] {
        let other = BODY.replace(from, to);
        assert_ne!(other, BODY);
        let text = format!("{}{}", function("one", BODY), function("two", &other));
        let kinds: Vec<Kind> = found(&[("a.rs", &text)]).into_iter().map(|(kind, _)| kind).collect();
        assert!(!kinds.contains(&Kind::Exact) && !kinds.contains(&Kind::Renamed), "{from}: {kinds:?}");
    }
}

#[test]
fn copied_statements_inside_other_functions_are_found_as_a_run() {
    let longer = BODY.replace("    total\n}", "    total.saturating_add(u64::from(first.is_empty()))\n}");
    let text = format!("{}{}", function("one", BODY), function("two", &longer));
    let groups = found(&[("a.rs", &text)]);
    assert_eq!(groups, vec![(Kind::Run, names(&["one", "two"]))]);
    // 関数は、自分自身の写しではない。
    assert_eq!(found(&[("a.rs", &function("one", BODY))]), vec![]);
    // 同じ文を一つの関数の中で繰り返すのも写しで、重ならない列ごとに一つと数える。
    let repeated = "fn only(k: &mut Vec<u64>) {\n".to_owned() + &"    k.push(k.len().try_into().unwrap_or(u64::MAX).saturating_mul(3).saturating_add(1));\n".repeat(12) + "}\n";
    let groups = found(&[("a.rs", &repeated)]);
    assert_eq!(groups.len(), 1);
    assert!(groups.iter().all(|(kind, members)| *kind == Kind::Run && members.len() >= 2 && members.iter().all(|name| name == "only")));
}

#[test]
fn test_code_counts_only_long_runs() {
    // 最初の二つの文だけが同じ。
    let other = BODY.replace("total > 65536", "total > 4096");
    let text = format!("{}{}", function("one", BODY), function("two", &other));
    let size = find([("a.rs", text.as_str())].into_iter()).first().map_or(0, |group| group.size);
    assert!(size >= Kind::Run.least() && size < TRIAL_RUN, "この試験の元の大きさ {size} が、二つの境の間に無い");
    // 同じ写しでも、試験の中では数えない: 試験のファイル、試験のモジュール、試験の関数。
    assert_eq!(found(&[("crates/x/tests/a.rs", &text)]), vec![]);
    assert_eq!(found(&[("crates/x/src/tests.rs", &text)]), vec![]);
    let module = format!("#[cfg(test)]\nmod tests {{\n{text}}}\n");
    assert_eq!(found(&[("a.rs", &module)]), vec![]);
    let marked = format!("#[test]\n{}#[test]\n{}", function("one", BODY), function("two", &other));
    assert_eq!(found(&[("a.rs", &marked)]), vec![]);
    // 試験でも、関数まるごとの写しは数える。
    let whole = format!("{}{}", function("one", BODY), function("two", BODY));
    assert_eq!(found(&[("crates/x/tests/a.rs", &whole)]), vec![(Kind::Exact, names(&["one", "two"]))]);
    // 試験でも、長い列は数える。
    let twice = BODY.replace("    total\n}", &format!("    let again = {BODY};\n    total.saturating_add(again)\n}}"));
    let twice_longer = twice.replace("total.saturating_add(again)", "total.saturating_sub(again)");
    let long = format!("{}{}", function("one", &twice), function("two", &twice_longer));
    let kinds: Vec<Kind> = found(&[("crates/x/tests/a.rs", &long)]).into_iter().map(|(kind, _)| kind).collect();
    assert!(kinds.contains(&Kind::Run), "{kinds:?}");
}

#[test]
fn the_same_table_and_the_same_text_are_found() {
    let row = "[3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97]";
    let tables = format!("const ONE: [u32; 24] = {row};\nstatic TWO: [u32; 24] = {row};\n");
    assert_eq!(found(&[("a.rs", &tables)]), vec![(Kind::Table, names(&["ONE", "TWO"]))]);
    // 要素が一つ違えば、別の表である。
    let other = format!("const ONE: [u32; 24] = {row};\nconst TWO: [u32; 24] = {};\n", row.replace("97", "101"));
    assert_eq!(found(&[("a.rs", &other)]), vec![]);

    let said = "同じ説明の文を二か所に書くと、片方だけが直される日が来る。そうなる前に、名前を付けた定数にして一か所へ置く";
    let texts = format!("fn one() -> &'static str {{\n    \"{said}\"\n}}\nfn two() -> String {{\n    format!(\"{said}\")\n}}\n");
    assert_eq!(found(&[("a.rs", &texts)]), vec![(Kind::Text, names(&["one", "two"]))]);
    // 説明の注釈が同じなのは、写しに数えない。短い文字列も数えない。
    let documented = format!("/// {said}\nfn one() -> &'static str {{\n    \"短い\"\n}}\n/// {said}\nfn two() -> &'static str {{\n    \"短い\"\n}}\n");
    assert_eq!(found(&[("a.rs", &documented)]), vec![]);
}

#[test]
fn copied_script_lines_are_found_and_boilerplate_is_not() {
    let steps = "cargo build --locked\ncargo test --locked\ncargo doc --locked\ncargo clippy --locked\n";
    let one = format!("#!/usr/bin/env bash\nset -euo pipefail\n{steps}");
    let two = format!("#!/usr/bin/env bash\nset -euo pipefail\necho start\n\n# 注釈\n{steps}");
    let groups = found(&[("a.sh", &one), ("b.sh", &two)]);
    assert_eq!(groups, vec![(Kind::Lines, names(&["cargo build --locked", "cargo build --locked"]))]);
    // ジョブごとに書くほかない枠だけが同じなら、写しではない。
    let job = |name: &str| format!("  {name}:\n    runs-on: ubuntu-latest\n    timeout-minutes: 20\n    steps:\n      - uses: actions/checkout@0000000000000000000000000000000000000000 # v4\n      - run: scripts/ci/{name}.sh\n");
    let workflow = format!("jobs:\n{}{}{}", job("one"), job("two"), job("three"));
    assert_eq!(found(&[("ci.yml", &workflow)]), vec![]);
    // 三行までの同じ並びは数えない。
    let short = "cargo build --locked\ncargo test --locked\ncargo doc --locked\n";
    assert_eq!(found(&[("a.sh", short), ("b.sh", short)]), vec![]);
}

#[test]
fn short_functions_are_not_copies() {
    let text = "struct A(u32, u32);\nstruct B(u32, u32);\nimpl A {\n    fn new(x: u32, y: u32) -> Self {\n        Self(x, y)\n    }\n    fn get(&self) -> u32 {\n        self.0\n    }\n}\nimpl B {\n    fn new(x: u32, y: u32) -> Self {\n        Self(x, y)\n    }\n    fn get(&self) -> u32 {\n        self.0\n    }\n}\n";
    assert_eq!(found(&[("a.rs", text)]), vec![]);
    // 構文として成り立たないファイルは見ない (コンパイラが報告する)。
    let broken = format!("{}{} fn (", function("one", BODY), function("two", BODY));
    assert_eq!(found(&[("a.rs", &broken)]), vec![]);
}

#[test]
fn the_known_list_names_every_copy_and_gives_a_reason() {
    let text = format!("{}{}", function("one", BODY), function("two", BODY));
    let groups = find([("a.rs", text.as_str())].into_iter());
    let group = groups.first().unwrap();
    // 道具が出す行に理由を書けば、その組が載ったことになる。
    let line = entry_line(group).replace("# 理由", "# 試験が実装と独立に持つ答えなので二つ持つ");
    let entries = known(&format!("# 注釈\n\n{line}\n")).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries.first().map(|entry| entry.line), Some(3));
    assert!(listed(group, entries.first().unwrap()));
    // 種類が違う行、写しの一つが載っていない行は、その組を載せたことにならない。
    let other_kind = known(&line.replace("exact", "renamed")).unwrap();
    assert!(!listed(group, other_kind.first().unwrap()));
    let other_site = known(&line.replace("a.rs#two", "a.rs#three")).unwrap();
    assert!(!listed(group, other_site.first().unwrap()));
    // 理由が無い行、場所が一つの行、種類が分からない行は受け付けない。
    for (bad, at) in [
        (entry_line(group).replace("\t# 理由", ""), 1),
        (entry_line(group), 1),
        (format!("# 注釈\n{}", line.replace("\ta.rs#two", "")), 2),
        (line.replace("exact", "same"), 1),
        (line.replace("a.rs#two", "a.rs"), 1),
    ] {
        assert_eq!(known(&bad).map_err(|(line, _)| line), Err(at), "{bad}");
    }
}
