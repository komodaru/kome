//! 行の網羅の判定 ([`crate::cover`]) の試験。

use crate::Repo;
use crate::cover::{UNCOVERED, check_cover, excused};

const FILE: &str = "crates/kome/src/pick.rs";
const SOURCE: &str = "pub fn pick(a: Option<u32>) -> Result<u32, Error> {\n    match a {\n        Some(value) => Ok(value),\n        None => fallback(),\n    }\n}\n\nfn fallback() -> Result<u32, Error> {\n    return Err(Error::Broken(Invariant::Pick));\n}\n\n#[cfg(test)]\nmod tests {\n    fn unused() {\n        let never = 1;\n    }\n}\n";

fn repo(list: &str) -> Repo {
    let mut repo = Repo::default();
    repo.edit(FILE, |_| SOURCE.to_owned());
    repo.edit("crates/kome/src/tests.rs", |_| "fn only_in_tests() {\n    let x = 1;\n}\n".to_owned());
    repo.edit(UNCOVERED, |_| list.to_owned());
    repo
}

/// (行, 通った回数) の並びから、llvm-cov が書くのと同じ形の記録を作る。
fn lcov(lines: &[(usize, u64)]) -> String {
    let mut out = format!("SF:/somewhere/{FILE}\n");
    for (line, count) in lines {
        out.push_str(&format!("DA:{line},{count}\n"));
    }
    out + "end_of_record\nSF:/somewhere/crates/kome/src/tests.rs\nDA:2,0\nend_of_record\nSF:/somewhere/crates/other/src/lib.rs\nDA:1,0\nend_of_record\n"
}

const ALL: [(usize, u64); 6] = [(1, 3), (2, 3), (3, 2), (4, 1), (8, 1), (9, 1)];

fn with(line: usize, count: u64) -> Vec<(usize, u64)> {
    ALL.iter().map(|(at, old)| if *at == line { (line, count) } else { (*at, *old) }).collect()
}

#[test]
fn every_line_passed_is_accepted() {
    assert_eq!(check_cover(&repo(""), &lcov(&ALL)), Vec::<String>::new());
    // 試験だけのコード (末尾の mod tests、tests.rs) と、kome の外のファイルは数えない。
    let mut more = ALL.to_vec();
    more.push((15, 0));
    assert_eq!(check_cover(&repo(""), &lcov(&more)), Vec::<String>::new());
    // 同じ行が二つの実行ファイルに現れたら、どちらかが通っていればよい。
    let twice = lcov(&with(3, 0)) + &lcov(&ALL);
    assert_eq!(check_cover(&repo(""), &twice), Vec::<String>::new());
}

#[test]
fn a_line_no_test_passes_is_reported_unless_it_is_listed() {
    let problems = check_cover(&repo(""), &lcov(&with(4, 0)));
    assert_eq!(problems.len(), 1);
    assert!(problems.first().is_some_and(|problem| problem.starts_with(&format!("{FILE}:4: ")) && problem.contains("None => fallback(),")), "{problems:?}");
    // 代わりに確かめているものを書けば、認める。
    let listed = format!("# 注釈\n{FILE}\tNone => fallback(),\t# 呼ぶ側の型が None を渡さないことを示している\n");
    assert_eq!(check_cover(&repo(&listed), &lcov(&with(4, 0))), Vec::<String>::new());
    // 別のファイルの同じ内容の行や、別の内容の行は、その行を認めたことにならない。
    for other in [listed.replace("pick.rs", "other.rs"), listed.replace("None => fallback(),", "None => fallback()")] {
        assert_eq!(check_cover(&repo(&other), &lcov(&with(4, 0))).len(), 2, "{other}");
    }
}

#[test]
fn a_reply_to_a_broken_invariant_needs_no_test() {
    assert_eq!(check_cover(&repo(""), &lcov(&with(9, 0))), Vec::<String>::new());
}

#[test]
fn a_listed_line_that_is_passed_now_must_leave_the_list() {
    let listed = format!("{FILE}\tNone => fallback(),\t# 呼ぶ側の型が None を渡さないことを示している\n");
    let problems = check_cover(&repo(&listed), &lcov(&ALL));
    assert_eq!(problems.len(), 1);
    assert!(problems.first().is_some_and(|problem| problem.starts_with(&format!("{UNCOVERED}:1: "))), "{problems:?}");
}

#[test]
fn a_file_that_was_not_measured_is_reported() {
    // 記録が空でも、関数のあるファイルが測られていなければ合格にしない。
    assert_eq!(check_cover(&repo(""), "").len(), 1);
    assert_eq!(check_cover(&repo(""), &format!("SF:/somewhere/{FILE}\nend_of_record\n")).len(), 1);
    // 試験のモジュールの行だけが測られていても同じ。
    assert_eq!(check_cover(&repo(""), &lcov(&[(15, 4)])).len(), 1);
    // 対象のファイルが一つも無いリポジトリも合格にしない。
    assert_eq!(check_cover(&Repo::default(), &lcov(&ALL)).len(), 1);
}

#[test]
fn the_list_needs_three_cells_and_a_reason() {
    let good = format!("{FILE}\tNone => fallback(),\t# 呼ぶ側の型が None を渡さないことを示している");
    assert_eq!(excused(&format!("# 注釈\n\n{good}\n")).map(|list| list.iter().map(|entry| entry.line).collect::<Vec<_>>()), Ok(vec![3]));
    for bad in [format!("{FILE}\tNone => fallback(),"), format!("{FILE}\tNone => fallback(),\t# 短い"), format!("{FILE}\tNone => fallback(),\t理由の印が無い説明をここに書いた"), format!("{good}\t余り")] {
        assert_eq!(excused(&bad).map_err(|(line, _)| line), Err(1), "{bad}");
        assert_eq!(check_cover(&repo(&bad), &lcov(&ALL)).len(), 1, "{bad}");
    }
}
