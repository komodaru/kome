//! 規則と、それぞれが検出しなければならない壊し方。

use std::collections::BTreeSet;

use crate::scan::{Scan, scan};
use crate::{Repo, Report, Rule};

const KOME_SRC: &str = "crates/kome/src/";
const KOME_MANIFEST: &str = "crates/kome/Cargo.toml";
const KERNEL_RS: &str = "crates/kome/src/kernel.rs";
const LEDGER: &str = "docs/invariants.toml";
const CI: &str = ".github/workflows/ci.yml";
const EXPECT: &str = "tla/expect.tsv";
const COMPILE_FAIL: &str = "docs/compile-fail.tsv";
const WORDS: &str = "crates/kome-lint/src/escape-words.txt";

/// すべての規則。
pub const RULES: &[Rule] = &[
    Rule {
        id: "no-deps",
        about: "kome は依存クレート・機能フラグ・ビルドスクリプトを持たない",
        check: no_deps,
        breaks: &[
            |repo| repo.edit(KOME_MANIFEST, |text| text.replace("[dependencies]\n", "[dependencies]\nlibc = \"1\"\n")),
            |repo| repo.append(KOME_MANIFEST, "[build-dependencies]\ncc = \"1\""),
            |repo| repo.append("crates/kome/build.rs", "fn main() {}"),
            |repo| repo.append(KOME_MANIFEST, "[features]\nstd = []"),
        ],
    },
    Rule {
        id: "no-std",
        about: "kome は no_std + alloc で、std を名指ししない",
        check: no_std,
        breaks: &[
            |repo| repo.edit("crates/kome/src/lib.rs", |text| text.replace("#![cfg_attr(not(test), no_std)]", "")),
            |repo| repo.append(KERNEL_RS, "fn now() -> std::time::Instant { std::time::Instant::now() }"),
        ],
    },
    Rule {
        id: "required-lints",
        about: "コンパイラと clippy の禁止事項を緩めない",
        check: required_lints,
        breaks: &[
            |repo| repo.edit(KOME_MANIFEST, |text| text.replace("arithmetic_side_effects = \"deny\"", "arithmetic_side_effects = \"warn\"")),
            |repo| repo.edit(KOME_MANIFEST, |text| text.replace("unsafe_code = \"forbid\"\n", "")),
            |repo| repo.edit("Cargo.toml", |text| text.replace("unwrap_used = \"deny\"\n", "")),
            |repo| repo.edit("crates/kome-testkit/Cargo.toml", |text| text.replace("[lints]\nworkspace = true", "")),
            |repo| repo.append("clippy.toml", "too-many-arguments-threshold = 99"),
        ],
    },
    Rule {
        id: "no-local-allow",
        about: "その場で lint を黙らせない (allow / expect / warn 属性を書かない)",
        check: no_local_allow,
        breaks: &[
            |repo| repo.append(KERNEL_RS, "#[allow(dead_code)]\nfn unused() {}"),
            |repo| repo.edit("crates/kome-testkit/src/lib.rs", |text| format!("#![allow(clippy::all)]\n{text}")),
            |repo| repo.append(KERNEL_RS, "#[cfg_attr(test, expect(dead_code))]\nfn unused() {}"),
            |repo| repo.append(KERNEL_RS, "macro_rules! quiet { () => { #[allow(dead_code)] fn unused() {} }; }"),
        ],
    },
    Rule {
        id: "host-vocabulary",
        about: "kome は特定の OS・ホスト・実行基盤の語彙を持たない",
        check: host_vocabulary,
        breaks: &[
            |repo| repo.append("crates/kome/src/error.rs", "/// 状態。\npub struct NtStatus(u32);"),
            |repo| repo.append(KERNEL_RS, "fn hwnd_of(window: u32) -> u32 { window }"),
            |repo| repo.append(KERNEL_RS, "fn grow(wasm_pages: u32) -> u32 { wasm_pages }"),
        ],
    },
    Rule {
        id: "no-globals",
        about: "kome は大域状態と内部可変性を持たない (状態は Kernel の値だけ)",
        check: no_globals,
        breaks: &[
            |repo| repo.append(KERNEL_RS, "static COUNT: u32 = 0;"),
            |repo| repo.append(KERNEL_RS, "fn shared(value: &core::cell::RefCell<u8>) -> u8 { *value.borrow() }"),
            |repo| repo.append(KERNEL_RS, "fn counted(value: &core::sync::atomic::AtomicU32) -> u32 { value.load(core::sync::atomic::Ordering::Relaxed) }"),
        ],
    },
    Rule {
        id: "raw-collections",
        about: "確保に失敗しうる入れ物は store.rs の包みだけを使う",
        check: raw_collections,
        breaks: &[
            |repo| repo.append(KERNEL_RS, "fn grow() -> alloc::vec::Vec<u8> { alloc::vec::Vec::new() }"),
            |repo| repo.append(KERNEL_RS, "fn name(id: u32) -> usize { alloc::format!(\"{id}\").len() }"),
        ],
    },
    Rule {
        id: "no-pub-fields",
        about: "kome の構造体はフィールドを公開しない (不変条件を作る関数だけが値を作る)",
        check: no_pub_fields,
        breaks: &[|repo| repo.append(KERNEL_RS, "/// 開いた型。\npub struct Open {\n    /// 値。\n    pub value: u32,\n}")],
    },
    Rule {
        id: "no-catch-all",
        about: "kome の match は `_` の腕で残りをまとめない",
        check: no_catch_all,
        breaks: &[|repo| repo.append(KERNEL_RS, "fn kind(value: u8) -> u8 { match value { 0 => 1, _ => 2 } }")],
    },
    Rule {
        id: "no-open-range",
        about: "kome は終わりの無い範囲を数え上げない (端で桁あふれして止まる)",
        check: no_open_range,
        breaks: &[|repo| repo.append(KERNEL_RS, "fn numbered(items: &[u8]) -> usize { (0u32..).zip(items).count() }")],
    },
    Rule {
        id: "escape-comments",
        about: "コメントに書いて実装を省かない",
        check: escape_comments,
        breaks: &[
            |repo| repo.append(KERNEL_RS, &format!("// {}: 後で直す", first_word())),
            |repo| repo.append("scripts/ci/tla.sh", &format!("# {}", first_word().to_uppercase())),
            |repo| repo.append("tla/WaitRace.tla", &format!("\\* {}", first_word())),
            |repo| repo.append(KERNEL_RS, &format!("{} 期限は{}ませる。\nfn unused() {{}}", "/".repeat(3), native_word())),
        ],
    },
    Rule {
        id: "compile-fail-kept",
        about: "型で防いだ誤りの compile_fail 試験を減らさない",
        check: compile_fail_kept,
        breaks: &[
            |repo| repo.edit("crates/kome/src/sched.rs", |text| text.replacen("```compile_fail", "```ignore", 1)),
            |repo| repo.edit("crates/kome/src/id.rs", |text| text.replacen("```compile_fail", "```", 1)),
            |repo| repo.remove(COMPILE_FAIL),
        ],
    },
    Rule {
        id: "no-skipped-tests",
        about: "試験を無視・条件つきで外さない (走らない試験は合格に数えない)",
        check: no_skipped_tests,
        breaks: &[
            |repo| repo.edit("crates/kome/src/tests.rs", |text| text.replacen("#[test]\n", &format!("#[test]\n#[{}]\n", skip_word()), 1)),
            |repo| repo.edit("crates/kome-testkit/tests/sim.rs", |text| text.replacen("    #[test]\n", &format!("    #[test]\n    #[{} = \"遅い\"]\n", skip_word()), 1)),
            |repo| repo.edit("crates/kome-testkit/tests/headless.rs", |text| text.replacen("#[test]\n", &format!("#[cfg_attr(miri, {})]\n#[test]\n", skip_word()), 1)),
        ],
    },
    Rule {
        id: "ledger",
        about: "不変条件の台帳・実装・試験・証明・モデルが食い違わない",
        check: ledger,
        breaks: &[
            |repo| repo.edit(LEDGER, |text| text.replacen("id = \"RUN-1\"", "id = \"RUN-9\"", 1)),
            |repo| repo.edit(LEDGER, |text| text.replacen("::audit_detects_two_permits_for_one_thread", "::gone", 1)),
            |repo| repo.edit(LEDGER, |text| text.replacen("a_removed_id_never_resolves_again", "a_proof_that_is_gone", 1)),
            |repo| repo.edit(LEDGER, |text| text.replacen("WaitRace:OneOutcome", "WaitRace:Nothing", 1)),
            |repo| repo.edit(LEDGER, |text| text.replacen("audit = \"audit_runs\"", "audit = \"audit_nothing\"", 1)),
            |repo| repo.edit("crates/kome/src/error.rs", |text| text.replacen("=> \"MEM-3\"", "=> \"MEM-4\"", 1)),
            |repo| repo.edit("crates/kome/src/proofs.rs", |text| text.replacen("#[kani::proof]\n", "", 1)),
        ],
    },
    Rule {
        id: "tla-listed",
        about: "TLA+ のモデルは全て実行され、実装から生成した表を使い、誤りの変種で反例が出る",
        check: tla_listed,
        breaks: &[
            |repo| repo.edit(EXPECT, |text| text.replace("IoLifecycleDuplicate.cfg\tIoLifecycle\tSettlesOnce\n", "")),
            |repo| repo.edit("tla/IoLifecycle.tla", |text| text.replace("EXTENDS IoTable, ", "EXTENDS ")),
            |repo| repo.edit(EXPECT, |text| text.lines().filter(|line| line.ends_with("\tok") || line.starts_with("IoLifecycle")).map(|line| format!("{line}\n")).collect()),
            |repo| repo.append("tla/WaitRace.tla", "WaitDecide == <<>>"),
        ],
    },
    Rule {
        id: "operations-covered",
        about: "Kernel の操作は全て Input にあり、決定的シミュレーションが呼ぶ",
        check: operations_covered,
        breaks: &[|repo| repo.append(KERNEL_RS, "impl Kernel {\n    /// 新しい操作。\n    pub fn fresh_operation(&mut self) {}\n}")],
    },
    Rule {
        id: "ci-required",
        about: "CI は全ての検証を実行し、失敗を見逃さない",
        check: ci_required,
        breaks: &[
            |repo| repo.edit(CI, |text| text.replace("scripts/ci/kani.sh", "true")),
            |repo| repo.edit(CI, |text| text.replace("scripts/ci/test.sh", "cargo test -p kome")),
            |repo| repo.edit(TEST_SH, |text| text.replace("cargo run -q --locked -p kome-lint -- ran \"$log\"\n", "")),
            |repo| repo.edit(TEST_SH, |text| text.replace("cargo test --workspace --locked", "cargo test -p kome --locked")),
            |repo| repo.edit(TEST_SH, |text| text.replace("set -euo pipefail", "set -eu")),
            |repo| repo.edit(CI, |text| text.replace("--target wasm32v1-none", "")),
            |repo| repo.edit(CI, |text| text.replacen("timeout-minutes:", "continue-on-error: true\n    timeout-minutes:", 1)),
            |repo| repo.edit(CI, |text| text.replace("cancel-in-progress: false", "cancel-in-progress: true")),
            |repo| repo.edit(CI, |text| text.replacen("actions/checkout@", "actions/checkout@v4 # ", 1)),
            |repo| repo.append("scripts/ci/tla.sh", "cargo fmt --all"),
            |repo| repo.append(".github/workflows/extra.yml", "jobs:\n  a:\n    steps:\n      - run: npm install"),
        ],
    },
];

fn kernel_files(repo: &Repo) -> impl Iterator<Item = (&str, &str)> {
    repo.under(KOME_SRC, ".rs").filter(|(path, _)| !path.ends_with("/tests.rs") && !path.ends_with("/proofs.rs"))
}

fn scanned(report: &mut Report, path: &str, text: &str, skip_tests: bool) -> Option<Scan> {
    match scan(text, skip_tests) {
        Ok(found) => Some(found),
        Err(error) => {
            report.add(path, 1, format!("構文を読めない: {error}"));
            None
        }
    }
}

fn table(report: &mut Report, repo: &Repo, path: &str) -> toml::Table {
    match repo.text(path).parse::<toml::Table>() {
        Ok(table) if repo.has(path) => table,
        Ok(_) => {
            report.add(path, 1, "ファイルが無い");
            toml::Table::new()
        }
        Err(error) => {
            report.add(path, 1, format!("TOML として読めない: {error}"));
            toml::Table::new()
        }
    }
}

fn no_deps(repo: &Repo, report: &mut Report) {
    let manifest = table(report, repo, KOME_MANIFEST);
    for key in ["dependencies", "dev-dependencies", "build-dependencies", "target", "features"] {
        let empty = manifest.get(key).is_none_or(|value| value.as_table().is_some_and(toml::Table::is_empty));
        if !empty {
            report.add_at(repo, KOME_MANIFEST, key, format!("kome は [{key}] を持たない。必要な機能は kome の外 (Personality・Host) に置く"));
        }
    }
    if repo.has("crates/kome/build.rs") {
        report.add("crates/kome/build.rs", 1, "kome はビルドスクリプトを持たない");
    }
    let lock = table(report, repo, "Cargo.lock");
    let packages = lock.get("package").and_then(toml::Value::as_array).map_or(&[][..], Vec::as_slice);
    for package in packages {
        let is_kome = package.get("name").and_then(toml::Value::as_str) == Some("kome");
        if is_kome && package.get("dependencies").is_some() {
            report.add_at(repo, "Cargo.lock", "name = \"kome\"", "Cargo.lock で kome が依存を持っている");
        }
    }
}

fn no_std(repo: &Repo, report: &mut Report) {
    let lib = "crates/kome/src/lib.rs";
    if !repo.text(lib).lines().any(|line| line == "#![cfg_attr(not(test), no_std)]") {
        report.add(lib, 1, "`#![cfg_attr(not(test), no_std)]` が無い");
    }
    for (path, text) in kernel_files(repo) {
        let Some(found) = scanned(report, path, text, true) else { continue };
        for (name, line) in &found.idents {
            if name == "std" {
                report.add(path, *line, "kome は std を使わない。core か alloc を使う");
            }
        }
    }
}

const KOME_RUST: [(&str, &str); 3] = [("unsafe_code", "forbid"), ("missing_docs", "deny"), ("unused_must_use", "deny")];
const WORKSPACE_RUST: [(&str, &str); 1] = [("unsafe_code", "forbid")];
const WORKSPACE_CLIPPY: [&str; 9] = [
    "all",
    "unwrap_used",
    "expect_used",
    "as_conversions",
    "wildcard_enum_match_arm",
    "match_wildcard_for_single_variants",
    "todo",
    "unimplemented",
    "dbg_macro",
];
const KOME_CLIPPY: [&str; 7] = [
    "panic",
    "unreachable",
    "indexing_slicing",
    "arithmetic_side_effects",
    "std_instead_of_core",
    "std_instead_of_alloc",
    "alloc_instead_of_core",
];
const CLIPPY_TOML: [&str; 4] =
    ["allow-unwrap-in-tests", "allow-expect-in-tests", "allow-indexing-slicing-in-tests", "allow-panic-in-tests"];

fn level<'a>(lints: Option<&'a toml::Value>, group: &str, name: &str) -> Option<&'a str> {
    let value = lints?.get(group)?.get(name)?;
    value.as_str().or_else(|| value.get("level").and_then(toml::Value::as_str))
}

fn required_lints(repo: &Repo, report: &mut Report) {
    let kome = table(report, repo, KOME_MANIFEST);
    let root = table(report, repo, "Cargo.toml");
    let kome_lints = kome.get("lints");
    let root_lints = root.get("workspace").and_then(|workspace| workspace.get("lints"));
    let mut need = |path: &str, lints: Option<&toml::Value>, group: &str, name: &str, want: &str| {
        if level(lints, group, name) != Some(want) {
            report.add_at(repo, path, name, format!("{group} の {name} は \"{want}\" でなければならない"));
        }
    };
    for (name, want) in KOME_RUST {
        need(KOME_MANIFEST, kome_lints, "rust", name, want);
    }
    for (name, want) in WORKSPACE_RUST {
        need("Cargo.toml", root_lints, "rust", name, want);
    }
    for name in WORKSPACE_CLIPPY {
        need("Cargo.toml", root_lints, "clippy", name, "deny");
        need(KOME_MANIFEST, kome_lints, "clippy", name, "deny");
    }
    for name in KOME_CLIPPY {
        need(KOME_MANIFEST, kome_lints, "clippy", name, "deny");
    }
    for (path, text) in repo.under("", "Cargo.toml") {
        // fuzz はワークスペースの外にあり、libFuzzer の入口が unsafe な属性を要るので継承できない。
        let member = path != "Cargo.toml" && path != KOME_MANIFEST && path != "fuzz/Cargo.toml";
        let inherits = text
            .parse::<toml::Table>()
            .ok()
            .and_then(|manifest| manifest.get("lints")?.get("workspace")?.as_bool())
            .unwrap_or(false);
        if member && !inherits {
            report.add(path, 1, "`[lints] workspace = true` でワークスペースの禁止事項を継承する");
        }
    }
    let clippy = table(report, repo, "clippy.toml");
    for key in clippy.keys() {
        if !CLIPPY_TOML.contains(&key.as_str()) {
            report.add_at(repo, "clippy.toml", key, format!("clippy.toml で {key} を変えない (試験の中の unwrap などを許す設定だけを置く)"));
        }
    }
}

fn no_local_allow(repo: &Repo, report: &mut Report) {
    for (path, text) in repo.under("", ".rs") {
        let Some(found) = scanned(report, path, text, false) else { continue };
        for (name, line) in &found.lint_attrs {
            report.add(path, *line, format!("`{name}` 属性で lint を黙らせない。指摘された箇所を直す"));
        }
    }
}

const HOST_WORDS: [&str; 34] = [
    "nt", "ntstatus", "hresult", "win", "win32", "win64", "windows", "winapi", "dword", "hwnd", "hmodule", "peb", "teb",
    "apc", "iocp", "overlapped", "registry", "dll", "wine", "linux", "posix", "unix", "errno", "epoll", "syscall", "wasm",
    "js", "browser", "webgpu", "opfs", "worklet", "jit", "x86", "x64",
];

/// 識別子を小文字の語に分ける (`NtStatus` と `nt_status` はどちらも `nt`, `status`)。
fn words(ident: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for c in ident.chars() {
        if c == '_' || (c.is_uppercase() && current.chars().last().is_some_and(char::is_lowercase)) {
            out.push(std::mem::take(&mut current));
        }
        if c != '_' {
            current.extend(c.to_lowercase());
        }
    }
    out.push(current);
    out.retain(|word| !word.is_empty());
    out
}

fn host_vocabulary(repo: &Repo, report: &mut Report) {
    for (path, text) in kernel_files(repo) {
        let Some(found) = scanned(report, path, text, true) else { continue };
        for (name, line) in &found.idents {
            // 呼び出すだけのメソッドの名前 (標準ライブラリの `windows` など) は kome の語彙ではない。
            if found.calls.contains(name) && !found.fns.iter().any(|(defined, _)| defined == name) {
                continue;
            }
            if let Some(word) = words(name).iter().find(|word| HOST_WORDS.contains(&word.as_str())) {
                report.add(path, *line, format!("`{name}` の `{word}` は特定の OS・ホストの語彙。kome の外 (Personality・Host) に置く"));
            }
        }
    }
}

const SHARED_STATE: [&str; 13] = [
    "Cell", "RefCell", "UnsafeCell", "OnceCell", "LazyCell", "Mutex", "RwLock", "Condvar", "OnceLock", "LazyLock", "Rc",
    "Arc", "Weak",
];

fn no_globals(repo: &Repo, report: &mut Report) {
    for (path, text) in kernel_files(repo) {
        let Some(found) = scanned(report, path, text, true) else { continue };
        for line in &found.statics {
            report.add(path, *line, "kome は static を持たない。状態は Kernel の値に置く");
        }
        for (name, line) in &found.macros {
            if name == "thread_local" || name == "lazy_static" {
                report.add(path, *line, format!("`{name}!` で大域状態を作らない"));
            }
        }
        for (name, line) in &found.idents {
            if SHARED_STATE.contains(&name.as_str()) || name.starts_with("Atomic") {
                report.add(path, *line, format!("`{name}` (共有・内部可変性) を使わない。`&mut Kernel` を通して変える"));
            }
        }
    }
}

const RAW_COLLECTIONS: [&str; 12] = [
    "Vec", "VecDeque", "Box", "BTreeMap", "BTreeSet", "HashMap", "HashSet", "BinaryHeap", "LinkedList", "String",
    "ToString", "ToOwned",
];

fn raw_collections(repo: &Repo, report: &mut Report) {
    for (path, text) in kernel_files(repo) {
        if path.ends_with("/store.rs") {
            continue;
        }
        let Some(found) = scanned(report, path, text, true) else { continue };
        for (name, line) in &found.idents {
            if RAW_COLLECTIONS.contains(&name.as_str()) {
                report.add(path, *line, format!("`{name}` を直接使わない。確保の失敗を返す store::List / store::Queue を使う"));
            }
        }
        for (name, line) in &found.macros {
            if name == "vec" || name == "format" {
                report.add(path, *line, format!("`{name}!` は確保に失敗すると止まる。store の包みを使う"));
            }
        }
    }
}

fn no_pub_fields(repo: &Repo, report: &mut Report) {
    for (path, text) in kernel_files(repo) {
        let Some(found) = scanned(report, path, text, true) else { continue };
        for (name, line) in &found.pub_fields {
            report.add(path, *line, format!("`{name}` のフィールドを公開しない。検査する関数で作り、読み出しはメソッドにする"));
        }
    }
}

fn no_catch_all(repo: &Repo, report: &mut Report) {
    for (path, text) in kernel_files(repo) {
        let Some(found) = scanned(report, path, text, true) else { continue };
        for line in &found.wild_arms {
            report.add(path, *line, "`_` の腕で残りをまとめない。場合を全て書く (増えたときにコンパイルが止まるように)");
        }
    }
}

fn no_open_range(repo: &Repo, report: &mut Report) {
    for (path, text) in kernel_files(repo) {
        let Some(found) = scanned(report, path, text, true) else { continue };
        for line in &found.open_ranges {
            report.add(path, *line, "`a..` は最後の値の次へ進むときに桁あふれする。`a..=最大値` と書く");
        }
    }
}

fn escape_words() -> impl Iterator<Item = &'static str> {
    include_str!("escape-words.txt").lines().filter(|line| !line.is_empty())
}

fn native_word() -> &'static str {
    escape_words().find(|word| !word.is_ascii()).unwrap_or("")
}

fn first_word() -> &'static str {
    escape_words().next().unwrap_or("")
}

/// ファイルのコメント部分を行ごとに返す。文書 (md) は全体をコメントとして扱う。
fn comments(path: &str, text: &str) -> Vec<(usize, String)> {
    let extension = path.rsplit('.').next().unwrap_or("");
    let (line_marks, block): (&[&str], Option<(&str, &str)>) = match extension {
        "rs" => (&["//"], Some(("/*", "*/"))),
        "tla" => (&["\\*"], Some(("(*", "*)"))),
        "cfg" => (&["\\*"], None),
        "toml" | "sh" | "yml" | "yaml" | "tsv" => (&["#"], None),
        "md" => (&[""], None),
        _ => (&[], None),
    };
    let mut out = Vec::new();
    let mut in_block = false;
    for (at, line) in text.lines().enumerate() {
        let number = at.saturating_add(1);
        if in_block {
            out.push((number, line.to_owned()));
            in_block = block.is_some_and(|(_, close)| !line.contains(close));
            continue;
        }
        if let Some(start) = line_marks.iter().filter_map(|mark| line.find(mark)).min() {
            out.push((number, line.get(start..).unwrap_or("").to_owned()));
        } else if let Some((open, close)) = block
            && let Some(start) = line.find(open)
        {
            let rest = line.get(start..).unwrap_or("");
            out.push((number, rest.to_owned()));
            in_block = !rest.contains(close);
        }
    }
    out
}

fn mentions(comment: &str, word: &str) -> bool {
    if !word.is_ascii() {
        return comment.contains(word);
    }
    let lower = comment.to_lowercase();
    lower.match_indices(word).any(|(at, _)| {
        let before = lower.get(..at).and_then(|head| head.chars().last());
        let after = lower.get(at.saturating_add(word.len())..).and_then(|tail| tail.chars().next());
        !before.is_some_and(|c| c.is_ascii_alphanumeric()) && !after.is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

fn escape_comments(repo: &Repo, report: &mut Report) {
    for (path, text) in repo.under("", "") {
        if path == WORDS || path == "Cargo.lock" {
            continue;
        }
        for (line, comment) in comments(path, text) {
            for word in escape_words() {
                if mentions(&comment, word) {
                    report.add(path, line, format!("コメントの「{word}」: 書いて済ませず、正しい動作を最後まで実装する"));
                }
            }
        }
    }
}

fn compile_fail_kept(repo: &Repo, report: &mut Report) {
    if !repo.has(COMPILE_FAIL) {
        report.add(COMPILE_FAIL, 1, "compile_fail 試験の一覧が無い");
    }
    let listed: Vec<(&str, usize)> = repo
        .text(COMPILE_FAIL)
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .filter_map(|line| line.split_once('\t'))
        .filter_map(|(path, count)| Some((path, count.parse().ok()?)))
        .collect();
    for (path, text) in repo.under(KOME_SRC, ".rs") {
        let count = text.matches("```compile_fail").count();
        let want = listed.iter().find(|(listed, _)| *listed == path).map_or(0, |(_, count)| *count);
        if count != want {
            report.add(path, 1, format!("compile_fail 試験が {count} 個 ({COMPILE_FAIL} では {want} 個)。減らさない。増やしたら一覧を直す"));
        }
        for fence in ["```ignore", "```no_run", "```should_panic", "```text"] {
            if let Some(at) = text.lines().position(|line| line.contains(fence)) {
                report.add(path, at.saturating_add(1), format!("`{fence}` の例は確かめられない。動く例か compile_fail にする"));
            }
        }
    }
    for (path, _) in &listed {
        if !repo.has(path) {
            report.add(COMPILE_FAIL, 1, format!("{path} が無い"));
        }
    }
}

/// 試験を外す属性の名前 (この規則のファイル自身に、その属性の形を書かないために組み立てる)。
fn skip_word() -> String {
    ["ig", "nore"].concat()
}

fn no_skipped_tests(repo: &Repo, report: &mut Report) {
    let word = skip_word();
    let files = repo.under("crates/", ".rs").chain(repo.under("fuzz/", ".rs"));
    for (path, text) in files {
        for (at, line) in text.lines().enumerate() {
            // 属性の中にこの語があれば、マクロの中 (proptest!) でも cfg_attr 越しでも見つける。
            let attribute = line.split_once("#[").or_else(|| line.split_once("#![")).map(|(_, rest)| rest);
            let skips = attribute.is_some_and(|rest| {
                rest.split(|c: char| !c.is_alphanumeric() && c != '_').any(|part| part == word)
            });
            if skips && !line.trim_start().starts_with("//") {
                report.add(path, at.saturating_add(1), "試験を外す属性は書かない。走らせられない試験は、走る形に直すか消す");
            }
        }
    }
}

fn has_fn(repo: &Repo, path: &str, name: &str) -> bool {
    repo.text(path).contains(&format!("fn {name}("))
}

fn strings(entry: &toml::Value, key: &str) -> Vec<String> {
    entry
        .get(key)
        .and_then(toml::Value::as_array)
        .map(|items| items.iter().filter_map(toml::Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
}

fn expectations(repo: &Repo) -> Vec<(&str, &str, &str)> {
    repo.text(EXPECT)
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .filter_map(|line| {
            let mut cells = line.split('\t');
            Some((cells.next()?, cells.next()?, cells.next()?))
        })
        .collect()
}

fn ledger(repo: &Repo, report: &mut Report) {
    let error_rs = "crates/kome/src/error.rs";
    let proofs_rs = "crates/kome/src/proofs.rs";
    let audit_rs = "crates/kome/src/audit.rs";
    let in_code: BTreeSet<(String, String)> = scanned(report, error_rs, repo.text(error_rs), true)
        .map(|found| found.string_arms.into_iter().collect())
        .unwrap_or_default();
    let proofs: Vec<(String, Vec<String>)> =
        scanned(report, proofs_rs, repo.text(proofs_rs), false).map(|found| found.fns).unwrap_or_default();
    let proven = |name: &str| proofs.iter().any(|(found, attrs)| found == name && attrs.iter().any(|attr| attr == "kani::proof"));
    let ledger = table(report, repo, LEDGER);
    let entries = ledger.get("invariant").and_then(toml::Value::as_array).map_or(&[][..], Vec::as_slice);
    let mut in_ledger = BTreeSet::new();
    let mut cited_proofs = BTreeSet::new();
    let expect = expectations(repo);
    for entry in entries {
        let field = |key: &str| entry.get(key).and_then(toml::Value::as_str).unwrap_or("");
        let id = field("id");
        let needle = format!("id = \"{id}\"");
        let mut bad = |message: String| report.add_at(repo, LEDGER, &needle, format!("{id}: {message}"));
        if !in_ledger.insert((field("variant").to_owned(), id.to_owned())) {
            bad("同じ項目が二つある".to_owned());
        }
        for key in ["statement", "scope"] {
            if field(key).trim().is_empty() {
                bad(format!("{key} が空"));
            }
        }
        if !repo.has(field("owner")) {
            bad(format!("owner のファイル {} が無い", field("owner")));
        }
        if entry.get("audit").is_some() {
            let audit = field("audit");
            let mentions_variant = repo.text(audit_rs).contains(&format!("Invariant::{}", field("variant")));
            if !has_fn(repo, audit_rs, audit) || !mentions_variant {
                bad(format!("audit.rs の {audit} がこの不変条件を数え直していない"));
            }
        }
        let mut tests = strings(entry, "tests");
        if tests.is_empty() {
            bad("tests が空".to_owned());
        }
        tests.push(field("caught").to_owned());
        for test in &tests {
            let found = test.split_once("::").is_some_and(|(path, name)| has_fn(repo, path, name));
            if !found {
                bad(format!("試験 {test} が無い"));
            }
        }
        for proof in strings(entry, "proofs") {
            if !proven(&proof) {
                bad(format!("proofs.rs に #[kani::proof] の {proof} が無い"));
            }
            cited_proofs.insert(proof);
        }
        for model in strings(entry, "models") {
            let Some((module, property)) = model.split_once(':') else {
                bad(format!("models の {model} は「モジュール:性質」と書く"));
                continue;
            };
            let defined = repo.text(&format!("tla/{module}.tla")).lines().any(|line| line.starts_with(&format!("{property} ==")));
            let checked = expect.iter().any(|(cfg, of, want)| {
                *of == module
                    && *want == "ok"
                    && repo.text(&format!("tla/{cfg}")).lines().any(|line| line.trim() == property)
            });
            let bites = expect.iter().any(|(_, of, want)| *of == module && *want == property);
            if !defined || !checked || !bites {
                bad(format!("{model}: モデルに定義があり、正しい設定で検査し、誤りの変種で破れる性質だけを書く"));
            }
        }
    }
    for (variant, id) in in_code.symmetric_difference(&in_ledger) {
        report.add_at(repo, LEDGER, &format!("id = \"{id}\""), format!("{id} ({variant}) が、台帳と error.rs の Invariant::code の片方にしか無い"));
    }
    for (name, attrs) in &proofs {
        if attrs.iter().any(|attr| attr == "kani::proof") && !cited_proofs.contains(name) {
            report.add_at(repo, proofs_rs, &format!("fn {name}("), format!("証明 {name} が台帳のどの不変条件にも載っていない"));
        }
    }
    let harness_like = proofs.iter().filter(|(_, attrs)| attrs.is_empty()).filter(|(name, _)| {
        // 補助関数 (any_* / some_* / outcome_of) 以外で属性の無い関数は、証明の付け忘れ。
        !name.starts_with("any_") && !name.starts_with("some_") && name != "outcome_of"
    });
    for (name, _) in harness_like {
        report.add_at(repo, proofs_rs, &format!("fn {name}("), format!("{name} に #[kani::proof] が無い"));
    }
}

fn tla_listed(repo: &Repo, report: &mut Report) {
    let expect = expectations(repo);
    for (path, _) in repo.under("tla/", ".cfg") {
        let name = path.trim_start_matches("tla/");
        if !expect.iter().any(|(cfg, _, _)| *cfg == name) {
            report.add(path, 1, format!("{EXPECT} に載っていない設定は実行されない"));
        }
    }
    let mut modules = BTreeSet::new();
    for (cfg, module, _) in &expect {
        modules.insert(*module);
        for file in [format!("tla/{cfg}"), format!("tla/{module}.tla")] {
            if !repo.has(&file) {
                report.add_at(repo, EXPECT, cfg, format!("{file} が無い"));
            }
        }
    }
    for (path, _) in repo.under("tla/", ".tla") {
        let module = path.trim_start_matches("tla/").trim_end_matches(".tla");
        let generated = module.ends_with("Table");
        if !generated && !modules.contains(module) {
            report.add(path, 1, format!("{EXPECT} に載っていないモデルは実行されない"));
        }
    }
    for module in &modules {
        let rows = || expect.iter().filter(|(_, of, _)| of == module);
        if !rows().any(|(_, _, want)| *want == "ok") || !rows().any(|(_, _, want)| *want != "ok") {
            report.add_at(repo, EXPECT, module, format!("{module}: 違反なしの設定と、反例の出る変種の両方が要る"));
        }
    }
    for (model, table, relation) in [("IoLifecycle", "IoTable", "IoStep"), ("WaitRace", "WaitTable", "WaitDecide")] {
        let path = format!("tla/{model}.tla");
        let text = repo.text(&path);
        let extends = text.lines().any(|line| line.starts_with("EXTENDS") && line.contains(table));
        let redefines = text.lines().any(|line| line.starts_with(&format!("{relation} ==")));
        if !extends || redefines {
            report.add_at(repo, &path, "EXTENDS", format!("{model} は {relation} を自分で書かず、実装から生成した {table} を使う"));
        }
        if !repo.text(&format!("tla/{table}.tla")).contains("手で書き換えない") {
            report.add(&format!("tla/{table}.tla"), 1, "実装から生成した表でなければならない");
        }
    }
}

fn camel(name: &str) -> String {
    name.split('_')
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
        })
        .collect()
}

fn operations_covered(repo: &Repo, report: &mut Report) {
    let sim = "crates/kome-testkit/src/sim.rs";
    let input = "crates/kome/src/input.rs";
    let calls: BTreeSet<String> =
        scanned(report, sim, repo.text(sim), true).map(|found| found.calls.into_iter().collect()).unwrap_or_default();
    let variants: BTreeSet<String> = scanned(report, input, repo.text(input), true)
        .map(|found| found.variants.into_iter().filter(|(of, _)| of == "Input").map(|(_, name)| name).collect())
        .unwrap_or_default();
    let mut operations = BTreeSet::new();
    for (path, text) in kernel_files(repo) {
        let Some(found) = scanned(report, path, text, true) else { continue };
        for (name, line) in &found.kernel_ops {
            operations.insert(camel(name));
            // `apply` は Input の入口、`poll_effect` は出力の取り出しで、どちらも Input の変種ではない。
            let is_port = name == "apply" || name == "poll_effect";
            if !is_port && !variants.contains(&camel(name)) {
                report.add(path, *line, format!("操作 {name} に対応する Input::{} が無い", camel(name)));
            }
            if name != "apply" && !calls.contains(name) {
                report.add(path, *line, format!("操作 {name} を決定的シミュレーション ({sim}) が呼んでいない"));
            }
        }
    }
    for variant in &variants {
        if !operations.contains(variant) {
            report.add_at(repo, input, &format!("{variant}("), format!("Input::{variant} に対応する Kernel の操作が無い"));
        }
    }
}

const CI_STEPS: [&str; 9] = [
    "cargo run -q --locked -p kome-lint",
    "cargo clippy --workspace --all-targets --locked -- -D warnings",
    "scripts/ci/test.sh",
    "cargo build -p kome --locked --target wasm32v1-none",
    "cargo build -p kome --locked --target thumbv7em-none-eabi",
    "cargo miri test -p kome",
    "scripts/ci/kani.sh",
    "scripts/ci/tla.sh",
    "cancel-in-progress: false",
];
/// 試験のスクリプトが必ず行うこと: 全クレートの試験と、走った試験の照合。
const TEST_SH: &str = "scripts/ci/test.sh";
const TEST_STEPS: [&str; 2] = ["cargo test --workspace --locked 2>&1 | tee \"$log\"", "cargo run -q --locked -p kome-lint -- ran \"$log\""];
const CI_BANNED: [(&str, &str); 6] = [
    ("continue-on-error", "失敗を見逃す設定を置かない"),
    ("|| true", "失敗を握りつぶさない"),
    ("cancel-in-progress: true", "main への push が続くと実行が終わらなくなる。取り消さない"),
    ("npm ", "npm は使わない"),
    ("npx ", "npm は使わない"),
    ("cargo fmt", "cargo fmt は使わない"),
];

fn ci_required(repo: &Repo, report: &mut Report) {
    let text = repo.text(CI);
    let runs: Vec<&str> = text.lines().map(str::trim).collect();
    for step in CI_STEPS {
        if !runs.iter().any(|line| !line.starts_with('#') && line.contains(step)) {
            report.add(CI, 1, format!("CI が `{step}` を実行していない"));
        }
    }
    for step in TEST_STEPS {
        if !repo.text(TEST_SH).lines().any(|line| line.trim() == step) {
            report.add(TEST_SH, 1, format!("試験のスクリプトが `{step}` を実行していない"));
        }
    }
    if !repo.text(TEST_SH).lines().any(|line| line.trim() == "set -euo pipefail") {
        report.add(TEST_SH, 1, "`set -euo pipefail` が無いと、試験の失敗が照合の前で消える");
    }
    let scripts = repo.under(".github/", "").chain(repo.under("scripts/", ""));
    for (path, text) in scripts {
        for (at, line) in text.lines().enumerate() {
            let number = at.saturating_add(1);
            let code = line.trim();
            if code.starts_with('#') {
                continue;
            }
            // scripts/ci の検査は、期待どおりの失敗 (反例) を自分で判定するために出力を取る。
            let captures = path.starts_with("scripts/") && code.contains("=\"$(") && code.ends_with("|| true)\"");
            for (banned, why) in CI_BANNED {
                if code.contains(banned) && !(banned == "|| true" && captures) {
                    report.add(path, number, format!("`{banned}`: {why}"));
                }
            }
            if let Some((_, action)) = code.split_once("uses: ") {
                let pinned = action.split_once('@').is_some_and(|(_, rest)| {
                    let revision = rest.split_whitespace().next().unwrap_or("");
                    revision.len() == 40 && revision.chars().all(|c| c.is_ascii_hexdigit())
                });
                if !pinned {
                    report.add(path, number, "Action はコミットのハッシュで固定する");
                }
            }
        }
    }
}
