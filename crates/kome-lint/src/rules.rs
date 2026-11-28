//! 規則と、それぞれが検出しなければならない壊し方。

use std::collections::BTreeSet;

use crate::scan::{Scan, scan};
use crate::{Keep, Repo, Report, Rule, dup};

const KOME_SRC: &str = "crates/kome/src/";
const KOME_MANIFEST: &str = "crates/kome/Cargo.toml";
const KERNEL_RS: &str = "crates/kome/src/kernel.rs";
const TIMER_RS: &str = "crates/kome/src/timer.rs";
const SIM_RS: &str = "crates/kome-testkit/src/sim.rs";
const MEMORY_MD: &str = "docs/memory.md";
const ARCHITECTURE_MD: &str = "docs/architecture.md";
const PROOFS_TSV: &str = "docs/proofs.tsv";
const LEDGER: &str = "docs/invariants.toml";
const CI: &str = ".github/workflows/ci.yml";
const EXPECT: &str = "tla/expect.tsv";
const COMPILE_FAIL: &str = "docs/compile-fail.tsv";
const TESTS_RS: &str = "crates/kome/src/tests.rs";
const PAGES_RS: &str = "crates/kome-testkit/src/pages.rs";
const PIPES_RS: &str = "crates/kome-testkit/src/pipes.rs";
const IPC_RS: &str = "crates/kome/src/ipc.rs";
const OBJECT_RS: &str = "crates/kome/src/object.rs";
const BACKEND_RS: &str = "crates/kome/src/backend.rs";
const HOST_RS: &str = "crates/kome-host-std/src/lib.rs";
const LOOM_RS: &str = "crates/kome-host-std/tests/loom.rs";
const LOOM_SH: &str = "scripts/ci/loom.sh";
const BENCH_RS: &str = "crates/kome-bench/benches/kernel.rs";
const BENCH_MD: &str = "docs/bench.md";
const INPUT_RS: &str = "crates/kome/src/input.rs";
const IO_RS: &str = "crates/kome/src/io.rs";
const FRESH_OPERATION: &str = "impl Kernel {\n    /// 新しい操作。\n    pub fn fresh_operation(&mut self) {}\n}";
const NOTIFY_CHECKED: &str = "通知先にする資源を can_notify で確かめていない (通信路の通知の資源は通知先にできない)";
const FINISHED_HOST_TEST: &str = "fn a_host_finished_from_the_start_lets_its_workers_go()";
const LATE_INTERRUPT_TEST: &str = "fn an_interrupt_for_one_run_never_stops_the_next_thread_on_the_same_worker()";
const SHADOW_CHECKED: &str = "self.space_changed(process, ";
const STALE_OR_FOREIGN: &str = "if self.includes(id.issuer) { Error::Stale(K::TAG) } else { Error::Foreign }";
const FOREIGN_REPLY: &str = "assert_eq!(own.k.apply(input), Err(Error::Foreign), \"{at}: {shown}\");";
/// 規則 `dup` の自己試験が足す関数。壊し方は、これと、これを写したものを足す。
const DUP_FN: &str = "fn dup_first(first: &[u64], second: &[u64]) -> u64 {\n    let mut total = 0u64;\n    for entry in first.iter().chain(second.iter()) {\n        if *entry > 16 && *entry < 4096 {\n            total = total.saturating_add(entry.saturating_mul(3)).saturating_sub(1);\n        }\n    }\n    if total > 65536 {\n        total = total.saturating_sub(first.len().try_into().unwrap_or(u64::MAX));\n    }\n    total\n}\n";
const DUP_TABLE: &str = "const DUP_ONE: [u32; 24] = [3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97];\nconst DUP_TWO: [u32; 24] = [3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97];";
const BENCH_GATE: &str = "docs/bench-gate.tsv";
const BENCH_LIB: &str = "crates/kome-bench/src/lib.rs";
const BENCH_SH: &str = "scripts/ci/bench-gate.sh";
const HOST_TESTS_RS: &str = "crates/kome-host-std/tests/host.rs";
const SIM_TESTS_RS: &str = "crates/kome-testkit/tests/sim.rs";
const EXEC_RS: &str = "crates/kome/src/exec.rs";
const VERUS_TSV: &str = "docs/verus.tsv";
const VERUS_SH: &str = "scripts/ci/verus.sh";
const LOOKUP_RS: &str = "crates/kome/src/lookup.rs";
const MEM_RS: &str = "crates/kome/src/mem.rs";
const FLUX_TSV: &str = "docs/flux.tsv";
const FLUX_SH: &str = "scripts/ci/flux.sh";
const ADDR_RS: &str = "crates/kome/src/addr.rs";
const EFFECT_RS: &str = "crates/kome/src/effect.rs";
const ID_RS: &str = "crates/kome/src/id.rs";
const SCHED_RS: &str = "crates/kome/src/sched.rs";
const RUNQ_RS: &str = "crates/kome/src/runq.rs";
const TRACE_RS: &str = "crates/kome-trace/src/lib.rs";
const TRACE_WORDS_RS: &str = "crates/kome-trace/src/words.rs";
const TRACE_TESTS_RS: &str = "crates/kome-trace/tests/cases/mod.rs";
const CONFORM_RS: &str = "crates/kome/src/conform.rs";
const CONFORM_MD: &str = "docs/tla-conformance.md";
const TLA_SH: &str = "scripts/ci/tla.sh";
const CAPACITY: &str = "docs/capacity.tsv";
const WORDS: &str = "crates/kome-lint/src/escape-words.txt";

/// すべての規則。
pub const RULES: &[Rule] = &[
    Rule {
        id: "no-deps",
        about: "kome は依存クレート・機能フラグ・ビルドスクリプトを持たない",
        check: no_deps,
        keeps: &[],
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
        keeps: &[],
        breaks: &[
            |repo| repo.edit("crates/kome/src/lib.rs", |text| text.replace("#![cfg_attr(not(test), no_std)]", "")),
            |repo| repo.append(KERNEL_RS, "fn now() -> std::time::Instant { std::time::Instant::now() }"),
        ],
    },
    Rule {
        id: "required-lints",
        about: "コンパイラと clippy の禁止事項を緩めない",
        check: required_lints,
        keeps: &[],
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
        keeps: &[],
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
        keeps: &[],
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
        keeps: &[],
        breaks: &[
            |repo| repo.append(KERNEL_RS, "static COUNT: u32 = 0;"),
            |repo| repo.append(KERNEL_RS, "#[cfg(any(test, target_pointer_width = \"64\"))]\npub static COUNT: u32 = 0;"),
            |repo| repo.append(KERNEL_RS, "#[cfg(not(all(test, kani)))]\nmod counters {\n    static COUNT: u32 = 0;\n}"),
            |repo| repo.append(KERNEL_RS, "fn shared(value: &core::cell::RefCell<u8>) -> u8 { *value.borrow() }"),
            |repo| repo.append(KERNEL_RS, "fn counted(value: &core::sync::atomic::AtomicU32) -> u32 { value.load(core::sync::atomic::Ordering::Relaxed) }"),
        ],
    },
    Rule {
        id: "raw-collections",
        about: "確保に失敗しうる入れ物は store.rs の包みだけを使う",
        check: raw_collections,
        keeps: &[],
        breaks: &[
            |repo| repo.append(KERNEL_RS, "fn grow() -> alloc::vec::Vec<u8> { alloc::vec::Vec::new() }"),
            |repo| repo.append(KERNEL_RS, "fn name(id: u32) -> usize { alloc::format!(\"{id}\").len() }"),
        ],
    },
    Rule {
        id: "no-pub-fields",
        about: "kome の構造体はフィールドを公開しない (不変条件を作る関数だけが値を作る)",
        check: no_pub_fields,
        keeps: &[],
        breaks: &[|repo| repo.append(KERNEL_RS, "/// 開いた型。\npub struct Open {\n    /// 値。\n    pub value: u32,\n}")],
    },
    Rule {
        id: "no-catch-all",
        about: "kome の match は `_` の腕で残りをまとめない",
        check: no_catch_all,
        keeps: &[],
        breaks: &[|repo| repo.append(KERNEL_RS, "fn kind(value: u8) -> u8 { match value { 0 => 1, _ => 2 } }")],
    },
    Rule {
        id: "no-open-range",
        about: "kome は終わりの無い範囲を数え上げない (端で桁あふれして止まる)",
        check: no_open_range,
        keeps: &[],
        breaks: &[|repo| repo.append(KERNEL_RS, "fn numbered(items: &[u8]) -> usize { (0u32..).zip(items).count() }")],
    },
    Rule {
        id: "escape-comments",
        about: "コメントに書いて実装を省かない",
        check: escape_comments,
        keeps: &[],
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
        keeps: &[],
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
        keeps: &[],
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
        keeps: &[],
        breaks: &[
            |repo| repo.edit(LEDGER, |text| text.replacen("id = \"RUN-1\"", "id = \"RUN-9\"", 1)),
            |repo| repo.edit(LEDGER, |text| text.replacen("::audit_detects_two_permits_for_one_thread", "::gone", 1)),
            |repo| repo.edit(LEDGER, |text| text.replacen("a_removed_id_never_resolves_again", "a_proof_that_is_gone", 1)),
            |repo| repo.edit(LEDGER, |text| text.replacen("WaitRace:OneOutcome", "WaitRace:Nothing", 1)),
            |repo| repo.edit(LEDGER, |text| text.replacen("audit = \"audit_runs\"", "audit = \"audit_nothing\"", 1)),
            |repo| repo.edit("crates/kome/src/error.rs", |text| text.replacen("=> \"MEM-3\"", "=> \"MEM-4\"", 1)),
            |repo| repo.edit("crates/kome/src/proofs.rs", |text| text.replacen("#[kani::proof]\n", "", 1)),
            |repo| repo.edit(PROOFS_TSV, |text| text.replace("instant_add_never_wraps\t-\t", "instant_add_never_overflows\t-\t")),
            |repo| repo.edit("crates/kome/src/proofs.rs", |text| text.replace("#[kani::unwind(4)]", "#[kani::unwind(9)]")),
            |repo| repo.edit(PROOFS_TSV, |text| text.replace("an_exhausted_slot_is_retired\t3\t小", "an_exhausted_slot_is_retired\t3\t \n# 小")),
            |repo| repo.edit(PROOFS_TSV, |text| text.replace("an_exhausted_slot_is_retired\t3\t小さな上限での証明。前提: ", "an_exhausted_slot_is_retired\t3\t")),
            |repo| repo.edit(LEDGER, |text| text.replace("proved = \"時刻の加算", "remark = \"時刻の加算")),
            |repo| repo.edit(LEDGER, |text| text.replace("proved = \"表を使う小さな上限での証明。前提: ", "proved = \"全ての値について示す。")),
            |repo| repo.edit(LEDGER, |text| text.replace("tied = \"受理する遷移の表 IoTable は", "tied = \"受理する遷移の表は")),
            |repo| repo.edit(LEDGER, |text| text.replace("tested = \"同じ資源を待つ待機を", "remark = \"同じ資源を待つ待機を")),
            |repo| repo.edit(LEDGER, |text| text.replacen("id = \"RUN-2\"", "id = \"RUN-2\"\nmodelled = \"実行待ち行列の全ての順序\"", 1)),
            |repo| repo.remove(PROOFS_TSV),
        ],
    },
    Rule {
        id: "tla-listed",
        about: "TLA+ のモデルは全て実行され、実装から生成した表を使い、誤りの変種で反例が出る",
        check: tla_listed,
        keeps: &[],
        breaks: &[
            |repo| repo.edit(EXPECT, |text| text.replace("IoLifecycleDuplicate.cfg\tIoLifecycle\tSettlesOnce\n", "")),
            |repo| repo.edit("tla/IoLifecycle.tla", |text| text.replace("EXTENDS IoTable, ", "EXTENDS ")),
            |repo| repo.edit(EXPECT, |text| text.lines().filter(|line| line.ends_with("\tok") || line.starts_with("IoLifecycle")).map(|line| format!("{line}\n")).collect()),
            |repo| repo.append("tla/WaitRace.tla", "WaitDecide == <<>>"),
            |repo| repo.edit("tla/ProcessExit.tla", |text| text.replace("EXTENDS ExitTable, ", "EXTENDS ")),
            |repo| repo.append("tla/ProcessExit.tla", "ExitFinish == <<>>"),
            |repo| repo.edit(EXPECT, |text| text.replace("ProcessExitKeepHandles.cfg\tProcessExit\tReleasedAtExit\n", "")),
        ],
    },
    Rule {
        id: "operations-covered",
        about: "Kernel の操作は全て Input にあり、決定的シミュレーションが呼ぶ",
        check: operations_covered,
        keeps: &[],
        breaks: &[
            |repo| repo.append(KERNEL_RS, FRESH_OPERATION),
            |repo| repo.edit("crates/kome-testkit/src/sim.rs", |text| text.replace("snapshot.restore(", "snapshot.instance_of(")),
        ],
    },
    Rule {
        id: "quiet-operations",
        about: "確保しないと約束した操作は、文書・確保を数える検査・決定的シミュレーションで同じ一覧になる",
        check: quiet_operations,
        keeps: &[],
        breaks: &[
            |repo| repo.edit(SIM_RS, |text| text.replace("self.quiet(|k| k.exit_process(", "self.plain(|k| k.exit_process(")),
            |repo| repo.edit(SIM_RS, |text| text.replace("let made = self.k.create_thread(process, start);", "let made = self.quiet(|k| k.create_thread(process, start));")),
            |repo| repo.edit(MEMORY_MD, |text| text.replace("`signal`、`interrupt_wait`、`advance`", "`signal`、`interrupt_wait`")),
            |repo| repo.edit(MEMORY_MD, |text| text.replace("| `arm_timer` | 期限の列 |", "| `arm_timer`、`advance` | 期限の列 |")),
            |repo| repo.append(KERNEL_RS, FRESH_OPERATION),
            |repo| repo.edit("checks/noalloc/tests/noalloc.rs", |text| text.replace("assert_eq!(count(), before, \"seed", "assert!(count() >= before, \"seed")),
            |repo| repo.edit("checks/noalloc/src/lib.rs", |text| text.replacen("        note();\n", "", 1)),
        ],
    },
    Rule {
        id: "capacity-audited",
        about: "確保なしで追加する入れ物は、容量を誰が確保するかが表にあり、検査が容量を確かめる",
        check: capacity_audited,
        keeps: &[],
        breaks: &[
            |repo| repo.edit(CAPACITY, |text| text.lines().filter(|line| !line.starts_with("running\t")).map(|line| format!("{line}\n")).collect()),
            |repo| repo.edit("crates/kome/src/audit.rs", |text| text.replace("\"実行待ちの列が、スレッドの数ぶんの容量を持っていない\"", "\"実行待ちの列が短い\"")),
            |repo| repo.append(KERNEL_RS, "fn late(kernel: &mut Kernel, entry: DueEntry) -> bool { kernel.due.push_within(entry).is_ok() }"),
            |repo| repo.edit("crates/kome/src/mem.rs", |text| text.replace("owner.space.limbo.reserve(", "owner.space.maps.reserve(")),
            |repo| repo.append(CAPACITY, "waiters\twait\tsignal\t同じ操作"),
            |repo| repo.edit(CAPACITY, |text| text.lines().filter(|line| !line.starts_with("bytes\t")).map(|line| format!("{line}\n")).collect()),
        ],
    },
    Rule {
        id: "independent-model",
        about: "独立したモデルは kome の判定を呼ばず (借りるのは実体の ID だけ)、決定的シミュレーションが見え方と項目のまとまりをモデルと比べる",
        check: independent_model,
        keeps: &[
            Keep { path: SIM_RS, needle: "let expected = model.expect(page, within, touch);", why: "決定的シミュレーションが、メモリの見え方をモデルの判定 (Pages::expect) と比べていない", broken: "let expected = actual;" },
        ],
        breaks: &[
            |repo| repo.edit(PAGES_RS, |text| text.replace("use kome::BackingId;", "use kome::{BackingId, Prot};")),
            |repo| repo.append(PAGES_RS, "fn judged(prot: kome::Prot) -> bool { prot.allows(kome::Access::Write) }"),
            |repo| repo.edit(SIM_RS, |text| text.replace("self.check_merged(process, first, count)?;", "")),
            |repo| repo.edit(SIM_RS, |text| text.replace("&& model.continues(page, PAGE)", "&& false")),
        ],
    },
    Rule {
        id: "channel-model",
        about: "通信路のモデルは kome を使わず、決定的シミュレーションが結果・中身・通知をモデルと比べる。通知の数を変えられるのは通信路だけ",
        check: channel_model,
        keeps: &[
            Keep { path: TIMER_RS, needle: "self.can_notify(", why: NOTIFY_CHECKED, broken: "self.can_pin(" },
            Keep { path: IO_RS, needle: "self.can_notify(", why: NOTIFY_CHECKED, broken: "self.can_pin(" },
            Keep { path: IPC_RS, needle: "let admitted = entry.spec.admit(len, entry.queued(), frames, parcel.is_some()).map_err(Error::Refused)?;", why: "send が、受け付けの判定 (ChannelSpec::admit) を通っていない", broken: "let admitted = Some(len);" },
            Keep { path: SIM_RS, needle: "        self.check_pipes()?;\n", why: "操作のたびに、通信路の中身と通知をモデルと比べる", broken: "" },
            Keep { path: SIM_RS, needle: "if got != put {", why: "送信の結果をモデルと比べる", broken: "if false {" },
            Keep { path: SIM_RS, needle: "if filled != want.as_slice() || parcel != carried {", why: "受け取ったバイト列と資源をモデルと比べる", broken: "if parcel != carried {" },
            Keep { path: SIM_RS, needle: "if levels != (Ok(u64::from(pipe.readable())), Ok(u64::from(pipe.writable()))) {", why: "読める・書けるの通知をモデルと比べる", broken: "if false {" },
            Keep { path: SIM_RS, needle: "if queued != Ok(wide(pipe.queued())?) || self.k.next_message_len(*channel) != Ok(next) {", why: "中身の量と次の長さをモデルと比べる", broken: "if false {" },
            Keep { path: SIM_RS, needle: "pipe.forget(process);", why: "プロセスの終了で、モデルの端を外す", broken: "" },
        ],
        breaks: &[
            |repo| repo.append(PIPES_RS, "fn sized(spec: kome::ChannelSpec) -> kome::ChannelSpec { spec }"),
            |repo| repo.edit(OBJECT_RS, |text| text.replacen("        if entry.driven {\n            return Err(Error::Refused(Refusal::DrivenObject));\n        }\n", "", 1)),
            |repo| repo.append(KERNEL_RS, "fn forced(kernel: &mut Kernel, object: crate::id::ObjectId) {\n    if let Some(entry) = kernel.objects.get_mut(object) {\n        entry.driven = false;\n    }\n}"),
        ],
    },
    Rule {
        id: "executor-contract",
        about: "止まった理由は一か所で判定し、文書の表と一致する。貸し出しは一つの入口を通り、決定的シミュレーションが貸す順序を自分の記録と比べる。実行器は、貸されている実行権でだけ実行を始め、拒否した実行権を呼び出した側へ返す",
        check: executor_contract,
        keeps: &[
            Keep { path: SCHED_RS, needle: "            stop_allowed(stop, interrupted, permit.budget, consumed).map_err(Error::Refused)?;\n", why: "relinquish が、受理の前に理由を確かめていない (stop_allowed)", broken: "" },
            Keep { path: SCHED_RS, needle: "        match after_stop(stop, kill) {", why: "実行権を返したスレッドの行き先を、理由と終了要求から決めていない (after_stop)", broken: "        match after_stop(stop, None) {" },
            Keep { path: SIM_RS, needle: "Err(kome::NotRun::Rejected { permit, loaded: None, error: Error::Foreign }) => permit,", why: "決定的シミュレーションが、別の実行インスタンスの実行権で実行を始めないことを確かめていない", broken: "Err(kome::NotRun::Rejected { permit, .. }) => permit," },
            Keep { path: TESTS_RS, needle: "fn a_permit_of_another_kernel_never_runs_and_comes_back_whole()", why: "別の Kernel の実行権が実行器を動かさずに返る試験が無い", broken: "fn a_permit_of_another_kernel()" },
            Keep { path: EXEC_RS, needle: "        Err(NotRelinquished::Rejected(permit, error)) => NotRun::Rejected { permit, loaded: None, error },", why: "hand_back が、受け取られなかった実行権を返していない", broken: "        Err(NotRelinquished::Rejected(_, error)) => NotRun::Failed(error)," },
            Keep { path: EXEC_RS, needle: "#[must_use = \"拒否された実行権は into_permit で取り戻す\"]\n#[derive(Debug)]\npub enum NotRun<C> {", why: "拒否の値 (NotRun) を捨てても警告が出ない、または複製できる", broken: "#[derive(Debug)]\npub enum NotRun<C> {" },
            Keep { path: SIM_RS, needle: "if self.rank(*other) > self.rank(picked) {", why: "優先度の高いスレッドを飛ばしていないこと", broken: "if false {" },
            Keep { path: SIM_RS, needle: "if self.rank(*other) == self.rank(picked) && *theirs < mine {", why: "同じ優先度の中で、先に実行待ちになったスレッドを飛ばしていないこと", broken: "if false {" },
            Keep { path: SIM_RS, needle: "if let Some(idle) = self.since.keys().find(|thread| self.open(**thread)) {", why: "貸せるスレッドがあるのに貸さないことが無いこと", broken: "if let Some(idle) = None::<&ThreadId> {" },
            Keep { path: SIM_RS, needle: "        self.note_ready();\n", why: "操作のたびに、貸せる状態のスレッドを記録すること", broken: "" },
            Keep { path: SIM_RS, needle: "let ready = self.k.thread_phase(*thread) == Ok(ThreadPhase::Ready) && self.k.suspend_count(*thread) == Ok(0);", why: "貸せる状態の判定", broken: "let ready = self.k.priority(*thread).is_ok();" },
        ],
        breaks: &[
            |repo| repo.edit(SCHED_RS, |text| text.replace("    /// ゲストが実行を譲った。スレッドは実行待ちの末尾へ戻る。\n    Yielded,", "    /// ゲストが実行を譲った。スレッドは実行待ちの末尾へ戻る。\n    Yielded,\n    /// 寝た。\n    Slept,")),
            |repo| repo.edit(ARCHITECTURE_MD, |text| text.replace("| `Stop::Fault` |", "| `Stop::Trap` |")),
            |repo| repo.edit(SCHED_RS, |text| text.replace("        self.lend(worker, at).map(Some)", "        self.ready.remove(at);\n        Ok(None)")),
            |repo| repo.edit("crates/kome/src/exec.rs", |text| text.replace("#[must_use = \"取り出した文脈は Contexts::save で戻さなければならない\"]\n#[derive(Debug)]", "#[derive(Debug, Clone)]")),
            |repo| repo.edit(EXEC_RS, |text| text.replace("        Err(NotRelinquished::Rejected(permit, error)) => return Err(hand_back(kernel, permit, stopped.consumed, error)),", "        Err(NotRelinquished::Rejected(_, error)) => return Err(NotRun::Failed(error)),")),
            |repo| repo.edit(EXEC_RS, |text| text.replacen("    if let Err(error) = kernel.lent(&permit) {\n", "    if let Err(error) = Ok::<(), Error>(()) {\n", 1)),
            |repo| repo.edit(EXEC_RS, |text| text.replace("        return Err(NotRun::Rejected { permit, loaded: Some(loaded), error });", "        return Err(NotRun::Failed(error));")),
        ],
    },
    Rule {
        id: "process-quota",
        about: "プロセスごとの上限を見る箇所は、プロセスへの割り当て (Quota) も見る",
        check: process_quota,
        keeps: &[
            Keep { path: SIM_RS, needle: "Ok(()) if after > quota => Err(", why: "決定的シミュレーションが、割り当てを超える受理を検出しない", broken: "Ok(()) if false => Err(" },
        ],
        breaks: &[
            |repo| repo.edit(OBJECT_RS, |text| text.replacen("let limit = self.holdings_limit(process);", "let limit = self.limits.holdings();", 1)),
            |repo| repo.edit("crates/kome/src/mem.rs", |text| text.replacen("u64::from(self.mappings_limit(process))", "u64::from(self.limits.mappings())", 1)),
            |repo| repo.edit("crates/kome/src/thread.rs", |text| text.replace(">= u64::from(owner.quota.threads()) {", ">= u64::MAX {")),
            |repo| repo.edit(IO_RS, |text| text.replace("if ios > owner.quota.ios() {", "if false {")),
            |repo| repo.edit("crates/kome/src/mem.rs", |text| text.replace("total <= owner.quota.mapped_bytes()", "total <= u64::MAX")),
            |repo| repo.edit("crates/kome/src/process.rs", |text| text.replace("self.limits.holdings().min(quota)", "self.limits.holdings()")),
        ],
    },
    Rule {
        id: "trace-complete",
        about: "記録と再生 (kome-trace) は Input と Reply の全ての項目を扱い、返答の行に状態のあらましを入れ、行ごとに照合値を確かめる",
        check: trace_complete,
        keeps: &[
            Keep { path: TRACE_RS, needle: "        if sum != format!(\"{expected:016x}\") {", why: "行ごとに照合値を、書いたときの綴りのまま比べる (数として読むと大文字や符号の書き換えが通る)", broken: "        if sum.len() != 16 || u64::from_str_radix(sum, 16).ok() != Some(expected) {" },
            Keep { path: TRACE_RS, needle: "        \"< effects={} ready={} running={} now={} alive={}/{}/{}/{}/{}/{}/{} {result:?}\",", why: "返答の行に、状態のあらましと返答を入れる", broken: "        \"< effects={} ready={} running={} now={} alive={}/{}/{}/{}/{}/{}/{}\"," },
            Keep { path: TRACE_RS, needle: "            None => self.out.line(&format!(\"? {input:?}\")),", why: "再生できない入力も、記録には残す", broken: "            None => {}" },
        ],
        breaks: &[
            |repo| repo.edit(TRACE_WORDS_RS, |text| text.replace("        \"DropTimer\" => {", "        \"DropTimers\" => {")),
            |repo| repo.edit(TRACE_TESTS_RS, |text| text.replace("\"DisarmTimer\", \"DropTimer\", \"Advance\",", "\"DisarmTimer\", \"Advance\",")),
            |repo| repo.edit(INPUT_RS, |text| text.replace("    /// [`Kernel::poll_effect`]\n    PollEffect,", "    /// [`Kernel::poll_effect`]\n    PollEffect,\n    /// 何もしない。\n    Idle,")),
            |repo| repo.edit(INPUT_RS, |text| text.replace("    /// 受信の結果。\n    Received(Received),", "    /// 受信の結果。\n    Received(Received),\n    /// 何も無い。\n    Nothing,")),
            |repo| repo.edit(TRACE_RS, |text| text.replace("        if recorded != replayed {", "        if recorded.len() != replayed.len() {")),
            |repo| repo.edit("crates/kome-trace/Cargo.toml", |text| text.replace("[dependencies]\nkome = { path = \"../kome\" }\n", "[dependencies]\nkome = { path = \"../kome\" }\nserde = \"1\"\n")),
            |repo| repo.remove("crates/kome-trace/tests/golden/basic.trace"),
        ],
    },
    Rule {
        id: "run-identity",
        about: "実行の番号は Kernel の中で一意で、worker への中断の求めは一回の実行だけを名指しする。仕事が終わったホストの worker は眠らない",
        check: run_identity,
        keeps: &[
            Keep { path: LOOM_RS, needle: FINISHED_HOST_TEST, why: "Loom: 仕事が最初から終わっているホスト", broken: "fn a_finished_host()" },
            Keep { path: LOOM_RS, needle: LATE_INTERRUPT_TEST, why: "Loom: 前の実行への中断の求めが次のスレッドを止めない", broken: "fn an_interrupt()" },
            Keep { path: HOST_TESTS_RS, needle: FINISHED_HOST_TEST, why: "実スレッド: 仕事が最初から終わっているホスト", broken: "fn a_finished_host()" },
            Keep { path: HOST_TESTS_RS, needle: LATE_INTERRUPT_TEST, why: "実スレッド: 前の実行への中断の求めが次のスレッドを止めない", broken: "fn an_interrupt()" },
            Keep { path: SCHED_RS, needle: "        let run_gen = self.seq;\n", why: "実行の番号を、Kernel の通し番号から取る (スレッドごとに数えない)", broken: "        let run_gen = entry.run_gen.saturating_add(1);\n" },
            Keep { path: SCHED_RS, needle: "        self.unqueue(at)?;\n        self.seq = seq;\n", why: "貸すたびに、通し番号を進める", broken: "        self.unqueue(at)?;\n" },
            Keep { path: HOST_RS, needle: "                if guard.closed {\n                    self.wake(&guard, before);\n                    return Ok(());\n                }\n", why: "worker は、頼まれた仕事を処理した後、眠る前に閉じたかを確かめ直す", broken: "" },
            Keep { path: HOST_RS, needle: "self.wanted.load(Ordering::SeqCst) == permit.run().get()", why: "中断の求めは、実行の番号が一致するときだけ見える", broken: "self.wanted.load(Ordering::SeqCst) != NO_INTERRUPT" },
            Keep { path: HOST_RS, needle: "wanted.ok_or(HostFault::NoWorker)?.ask(run);", why: "中断の求めは、Effect::Interrupt が名指しした実行の番号で立てる", broken: "wanted.ok_or(HostFault::NoWorker)?.ask(kome::RunGen::new(0));" },
            Keep { path: SIM_RS, needle: "        self.check_interrupts()?;\n", why: "決定的シミュレーション: 操作のたびに、参照ホストの表現で中断の求めを確かめる", broken: "" },
            Keep { path: SIM_RS, needle: "            if seen != held.interrupted {", why: "決定的シミュレーション: 求めが見えることと、Kernel がその実行へ要求したことが一致する", broken: "            if seen && !held.interrupted && false {" },
            Keep { path: SIM_RS, needle: "?.ask(run);", why: "決定的シミュレーション: 参照ホストと同じ置き場 (InterruptSlot) へ求めを立てる", broken: "?.ask(held.permit.run());" },
            Keep { path: TESTS_RS, needle: "fn a_run_number_names_one_run_of_one_thread()", why: "実行の番号がスレッドをまたいで重ならない試験", broken: "fn a_run_number()" },
        ],
        breaks: &[
            |repo| repo.edit(SIM_TESTS_RS, |text| text.replace("        \"run:other-thread-after-interrupt\",\n", "")),
        ],
    },
    Rule {
        id: "tla-conform",
        about: "TLA+ のモデルの状態グラフの全体を、実際の Kernel の動きと突き合わせる。モデルの変数は、どれも実装の値との対応を持つ",
        check: tla_conform,
        keeps: &[
            Keep { path: CONFORM_RS, needle: "            if !model.edges.contains(&(from, to)) {", why: "実装の遷移がモデルにあることを確かめる", broken: "            if false {" },
            Keep { path: CONFORM_RS, needle: "    never_taken(&model, &taken).unwrap_or_else(|why| panic!(\"{}: {why}\", D::NAME));\n", why: "モデルの遷移を実装が全て行うことを確かめる", broken: "" },
            Keep { path: TLA_SH, needle: "        dump=(-dump dot \"$graphs/$module.dot\")\n", why: "正しい設定の状態グラフを書き出す", broken: "" },
        ],
        breaks: &[
            |repo| repo.edit(CONFORM_RS, |text| text.replace("const NAME: &'static str = \"WaitRace\";", "const NAME: &'static str = \"WaitRaces\";")),
            |repo| repo.edit(CONFORM_RS, |text| text.replace("format!(\"woken = {}\"", "format!(\"awake = {}\"")),
            |repo| repo.edit(TLA_SH, |text| text.replace("--lib conform:: --", "--lib conform::the_io --")),
            |repo| repo.edit(TLA_SH, |text| text.replace("        failed=1\n    fi\ndone\nexit", "    fi\ndone\nexit")),
            |repo| repo.edit(CONFORM_MD, |text| text.replace("| `lateGrant` |", "| `late` |")),
            |repo| repo.edit("tla/ProcessExit.tla", |text| text.replace("    drained    \\* ", "    extra,\n    drained    \\* ")),
            |repo| repo.edit(CONFORM_MD, |text| text.replace("### IoLifecycle", "### I/O")),
        ],
    },
    Rule {
        id: "run-queue-kept",
        about: "実行権を貸す相手は実行待ちの列の先頭から一手で選び、列の中の位置は runq.rs だけが書く。スレッドを全て見て選ぶ形へ戻さない",
        check: run_queue_kept,
        keeps: &[
            Keep { path: SCHED_RS, needle: "        let at = entry.queued.ok_or(Error::Refused(Refusal::Barrier))?;", why: "名指しの貸し出しは、スレッドが持つ位置で引く", broken: "        let at = self.ready.position(|waiting| waiting.thread == thread).ok_or(Error::Refused(Refusal::Barrier))?;" },
            Keep { path: BENCH_RS, needle: "for waiting in [16u64, 1024] {", why: "実行待ちが少ないときと多いときの貸し出しを測る", broken: "for waiting in [16u64] {" },
            Keep { path: TESTS_RS, needle: "fn the_run_queue_lends_in_the_same_order_as_a_scan_of_every_thread() {", why: "全てのスレッドを見て選ぶ手本と、貸す順序を比べる試験", broken: "fn the_run_queue_lends() {" },
        ],
        breaks: &[
            |repo| repo.edit(SCHED_RS, |text| text.replace("        let at = 0;\n        if self.ready.is_empty() {", "        let at = self.ready.iter().count().saturating_sub(1);\n        if self.ready.is_empty() {")),
            |repo| repo.append("crates/kome/src/thread.rs", "fn forget(kernel: &mut Kernel, thread: ThreadId) {\n    if let Some(entry) = kernel.threads.get_mut(thread) {\n        entry.queued = None;\n    }\n}"),
            |repo| repo.append(SCHED_RS, "fn jump(kernel: &mut Kernel, thread: ThreadId) {\n    if let Some(entry) = kernel.threads.get_mut(thread) {\n        entry.turn = 0;\n    }\n}"),
            |repo| repo.append(MEM_RS, "fn shove(kernel: &mut Kernel, waiting: crate::runq::Waiting) {\n    let _ = kernel.ready.push(waiting);\n}"),
            |repo| repo.edit(MEM_RS, |text| text.replace("            self.hold_back(process)?;\n", "")),
            |repo| repo.edit(SCHED_RS, |text| text.replace("                self.let_in(process)?;\n", "")),
            |repo| repo.edit(RUNQ_RS, |text| text.replace("        for _ in 0..usize::BITS {", "        loop {")),
        ],
    },
    Rule {
        id: "host-single-writer",
        about: "参照ホストは錠の外でだけゲストを実行し、同期の部品を Loom に差し替えられる形で使い、Loom がその進み方を試す",
        check: host_single_writer,
        keeps: &[
            Keep { path: HOST_RS, needle: "            #[cfg(not(loom_broken))]\n            self.wake(&guard, before);\n", why: "ゲストが止まった後に、待っている者を起こす", broken: "" },
            Keep { path: HOST_RS, needle: "        self.wake(&guard, before);\n        drop(guard);\n        settled.map(|_| result)", why: "外からの操作の後に、待っている者を起こす", broken: "        drop(guard);\n        settled.map(|_| result)" },
            Keep { path: HOST_RS, needle: "state.closed || state.kernel.ready_count() != 0 || state.kernel.next_deadline() != before;", why: "眠っている worker は、閉じたとき、貸せるスレッドがあるかもしれないとき、期限が変わったときに起こす", broken: "state.closed || state.kernel.ready_count() != 0;" },
            Keep { path: HOST_RS, needle: "        guard.idle = guard.idle.saturating_add(1);\n", why: "眠る worker を、錠の中で数えてから眠らせる", broken: "" },
            Keep { path: HOST_RS, needle: "            guard.watchers = guard.watchers.saturating_add(1);\n", why: "待つ者を、錠の中で数えてから待たせる", broken: "" },
            Keep { path: HOST_RS, needle: "                guard = self.sleep(guard)?;", why: "貸せるスレッドが無ければ、錠を放すのと待ち始めるのを一つの操作で行う", broken: "                drop(guard);\n                guard = self.lock()?;" },
            Keep { path: LOOM_SH, needle: "    if ! grep -q \"^test $name ... ok$\" \"$log\"; then\n", why: "書いてある検査が全て走ったことを照合する", broken: "    if false; then\n" },
        ],
        breaks: &[
            |repo| repo.edit(HOST_RS, |text| text.replace("            drop(guard);\n            let stopped = executor.run(", "            let stopped = executor.run(")),
            |repo| repo.append(HOST_RS, "fn other() -> std::sync::Mutex<u8> { std::sync::Mutex::new(0) }"),
            |repo| repo.append("crates/kome-host-std/src/store.rs", "use std::sync::atomic::AtomicBool;"),
            |repo| repo.append(HOST_RS, "fn back(kernel: &mut Kernel, permit: RunPermit) { let _ = kernel.relinquish(permit, kome::Stop::Trap, kome::Budget::new(0)); }"),
            |repo| repo.edit(LOOM_RS, |text| text.replace("    explore(2, 2, 0, Some(7));", "    explore(1, 2, 0, Some(7));")),
            |repo| repo.edit(LOOM_RS, |text| text.replace("    explore(2, 1, 1, Some(7));", "    explore(2, 1, 1, Some(5));")),
            |repo| repo.edit(LOOM_RS, |text| text.replace("        host.with(|state| state.kernel.unsuspend_thread(guest)).unwrap().unwrap();\n        host.wait_until", "        host.wait_until")),
            |repo| repo.edit(LOOM_RS, |text| text.replace("        support::check(&host, &made, yields);\n", "")),
            |repo| repo.edit(LOOM_SH, |text| text.replace("--cfg loom_broken", "")),
            |repo| repo.edit(LOOM_SH, |text| text.replace("if ! grep -q \"deadlock\" \"$log\"; then", "if false; then")),
            |repo| repo.edit("crates/kome-host-std/src/sync.rs", |text| text.replace("#[cfg(loom)]\n", "#[cfg(any())]\n")),
        ],
    },
    Rule {
        id: "bench-listed",
        about: "測定する操作は全て実行され、文書の表に載っている。記録と一つ前のコミットより遅くなっていないことを、基準の仕事との比で判定する",
        check: bench_listed,
        keeps: &[
            Keep { path: CI, needle: "          fetch-depth: 2\n", why: "test ジョブが、一つ前のコミットを取っていない (性能の判定が比べる相手)", broken: "" },
            Keep { path: BENCH_LIB, needle: "pub const LIMIT: f64 = 2.0;", why: "記録との比較の幅", broken: "pub const LIMIT: f64 = 200.0;" },
            Keep { path: BENCH_LIB, needle: "if ratio <= recorded * limit {", why: "記録の比の幅を超えたら、遅くなったと判定する (測れなかった値は収まったと見なさない)", broken: "if ratio <= recorded * limit || ratio.is_nan() {" },
            Keep { path: BENCH_LIB, needle: "pub const AGAINST_LIMIT: f64 = 1.15;", why: "一つ前のコミットとの比較の幅", broken: "pub const AGAINST_LIMIT: f64 = 9.0;" },
            Keep { path: BENCH_SH, needle: "cargo bench -p kome-bench --locked -- --gate docs/bench-gate.tsv\n", why: "記録と比べる", broken: "" },
            Keep { path: BENCH_SH, needle: "    if cargo bench -p kome-bench --locked -- --record \"$work/head.tsv\" --against \"$work/base.tsv\"; then", why: "今のコミットの最小値を足し込み、一つ前のコミットと比べる", broken: "    if true; then" },
            Keep { path: BENCH_SH, needle: "echo \"bench-gate: 六回ぶんの最小値でも、一つ前のコミット $base より遅い\" >&2\nexit 1\n", why: "測り足しても遅ければ失敗する", broken: "" },
            Keep { path: BENCH_LIB, needle: "if *least < *kept || kept.is_nan() { *least } else { *kept }", why: "測り足すときは、操作ごとに小さい方を残す", broken: "*least" },
            Keep { path: BENCH_SH, needle: "base=\"$(git rev-parse --verify 'HEAD^')\"\n", why: "一つ前のコミットが無ければ失敗する", broken: "base=\"$(git rev-parse --verify 'HEAD^')\" || exit 0\n" },
            Keep { path: BENCH_LIB, needle: "            None => Verdict::NotMeasured,", why: "記録にある操作を測っていなければ、失敗にする", broken: "            None => Verdict::Within(0.0, recorded)," },
        ],
        breaks: &[
            |repo| repo.edit(BENCH_RS, |text| text.replace("    timers(&bench);\n", "")),
            |repo| repo.edit(BENCH_MD, |text| text.replace("| 待機を始めて通知で起こす |", "| 待機 |")),
            |repo| repo.edit(BENCH_RS, |text| text.replace("\"64 バイトのメッセージを送って受け取る\"", "\"メッセージの往復\"")),
            |repo| repo.edit("crates/kome-bench/Cargo.toml", |text| text.replace("harness = false", "harness = true")),
            |repo| repo.edit(BENCH_GATE, |text| text.replace("タイマを仕掛けて期限まで進める\t", "タイマ\t")),
            |repo| repo.edit(BENCH_GATE, |text| text.replace("アドレスを引く (マッピング 128 個)\t", "# アドレスを引く (マッピング 128 個)\t")),
            |repo| repo.remove(BENCH_GATE),
            |repo| repo.edit(BENCH_LIB, |text| text.replace("&least, reference, AGAINST_LIMIT, false)", "&least, reference, LIMIT, false)")),
            |repo| repo.edit(BENCH_SH, |text| text.replace("$2 > recorded[$1] * 1.15 { print $1 }", "$2 != recorded[$1] { print $1 }")),
            |repo| repo.edit(BENCH_RS, |text| text.replace("    if let Err(why) = bench.finish() {", "    if let Err(why) = Ok::<(), String>(()) {")),
        ],
    },
    Rule {
        id: "verus-kept",
        about: "Verus の証明を減らさず、確かめずに信じる定義を増やさず、証明した関数を実際に使う",
        check: verus_kept,
        keeps: &[
            Keep { path: MEM_RS, needle: "        lookup::containing(self.maps.as_slice(), addr).and_then(|at| self.maps.get(at))", why: "アドレスを引く経路が、証明した関数を使っていない", broken: "        self.maps.iter().find(|mapping| mapping.start <= addr && addr <= mapping.last)" },
            Keep { path: MEM_RS, needle: "        lookup::first_ending_at_or_after(self.maps.as_slice(), addr)", why: "アドレスを引く経路が、証明した関数を使っていない", broken: "        self.maps.as_slice().partition_point(|mapping| mapping.last < addr)" },
            Keep { path: VERUS_SH, needle: "if [ -z \"$listed\" ] || [ \"$(proved \"$out/clean.json\")\" != \"$listed\" ]; then", why: "証明した関数が一覧とちょうど同じであることを確かめる", broken: "if false; then" },
        ],
        breaks: &[
            |repo| repo.edit(LOOKUP_RS, |text| {
                // `containing` の仕様を丸ごと取り除く。
                let (before, rest) = text.split_once("/// `addr` を含むマッピングの位置。\n#[cfg_attr(verus_keep_ghost, verus_spec(found =>").unwrap_or((text, ""));
                let (_, after) = rest.split_once("))]\n").unwrap_or(("", ""));
                format!("{before}{after}")
            }),
            |repo| repo.edit(VERUS_TSV, |text| text.replace("kome::lookup::containing\tverified", "kome::lookup::contains\tverified")),
            |repo| repo.append(LOOKUP_RS, "#[cfg_attr(verus_keep_ghost, verus_verify(external_body), verus_spec(r => ensures r == 0))]\nfn zero() -> u64 { 0 }"),
            |repo| repo.append(LOOKUP_RS, "#[cfg(verus_keep_ghost)]\nverus! { proof fn anything() ensures false { assume(false); } }"),
            |repo| repo.append(LOOKUP_RS, &format!("#[cfg(verus_keep_ghost)]\nverus! {{ proof fn anything() ensures false {{ {}(); }} }}", ["ad", "mit"].concat())),
            |repo| repo.edit(LOOKUP_RS, |text| text.replace("pub(crate) fn verus_broken_lookup(", "pub(crate) fn lookup_twice(")),
            |repo| repo.edit(VERUS_SH, |text| text.replace(" --cfg verus_broken ", " ")),
            |repo| repo.remove(VERUS_TSV),
        ],
    },
    Rule {
        id: "refinements-kept",
        about: "篩型 (Flux) の仕様を減らさず、書いた仕様は必ず確かめ、確かめずに信じる定義を増やさない",
        check: refinements_kept,
        keeps: &[
            Keep { path: "crates/kome/src/lib.rs", needle: "#[cfg(flux)]\nextern crate flux_core;", why: "Flux で確かめるときに flux_core を読み込まないと、checked_add などの結果が分からず仕様が通らない", broken: "" },
            Keep { path: FLUX_SH, needle: "export PATH=\"$sysroot/bin:$PATH\"\n", why: "cargo-flux を sysroot から探す (CI がキャッシュから戻すのは sysroot だけ)", broken: "" },
        ],
        breaks: &[
            |repo| repo.edit(ADDR_RS, |text| text.replacen("    #[cfg_attr(flux, flux::spec(fn(GuestRange[@range], GuestRange[@other]) -> bool[range.start <= other.last && other.start <= range.last]))]\n", "", 1)),
            |repo| repo.edit(ID_RS, |text| text.replacen("#[cfg_attr(flux, flux::spec(", "#[cfg_attr(flux, flux::trusted)]\n    #[cfg_attr(flux, flux::spec(", 1)),
            |repo| repo.append("crates/kome/src/time.rs", "#[cfg_attr(flux, flux::spec(fn(a: u64) -> u64[a]))]\nfn same(a: u64) -> u64 { a }"),
            |repo| repo.append(TIMER_RS, &format!("#[cfg_attr(flux, flux::{}nore)]\nfn unseen() {{}}", "ig")),
            |repo| repo.edit(KOME_MANIFEST, |text| text.replace("\"def:effect::Seats\", ", "")),
            |repo| repo.edit(KOME_MANIFEST, |text| text.replace("check-overflow = \"strict\"\n", "")),
            |repo| repo.edit(KOME_MANIFEST, |text| text.replace("[package.metadata.flux]\nenabled = true", "[package.metadata.flux]\nenabled = false")),
            |repo| repo.edit(EFFECT_RS, |text| text.replace("    fn flux_broken_room(", "    fn room_twice(")),
            |repo| repo.edit(FLUX_SH, |text| text.replace("--cfg flux_broken", "")),
            |repo| repo.edit(FLUX_SH, |text| text.replace("    exit 1\n", "")),
            |repo| repo.remove(FLUX_TSV),
        ],
    },
    Rule {
        id: "backend-boundary",
        about: "backend との境界は公開の問い合わせだけで書き、決定的シミュレーションは通知とアクセスの結果をモデルと比べる",
        check: backend_boundary,
        keeps: &[
            Keep { path: SIM_RS, needle: "        self.check_space(process)?;\n        self.check_shadow(process)\n    }", why: "アドレス空間を変えた後の確認が、通知の結果 (check_shadow) まで見ていない", broken: "        self.check_space(process)\n    }" },
            Keep { path: BACKEND_RS, needle: "            MapChange::Unmapped { .. } | MapChange::Protected { .. } => true,", why: "外す変更と権限の変更で、コードを捨てる", broken: "            MapChange::Unmapped { .. } | MapChange::Protected { .. } => false," },
            Keep { path: BACKEND_RS, needle: "    walk(kernel, process, addr, data.len(), Access::Write, |_, _, _| Ok(()))?;\n", why: "書き込みは、範囲の全体が書けることを先に確かめる", broken: "" },
            Keep { path: BACKEND_RS, needle: "    if bytes != 0 {\n        observer.changed(", why: "外したものがあるときだけ知らせる", broken: "    if false {\n        observer.changed(" },
            Keep { path: SIM_RS, needle: "if dropped.bytes != code || dropped.forgotten != 0 {", why: "捨てさせたコードの範囲を、モデルのページから引いたものと比べる", broken: "if dropped.forgotten != 0 {" },
            Keep { path: SIM_RS, needle: "let mut flaky = Flaky { store: &mut self.store, left: breaks };", why: "backend が途中のバイトで失敗する書き込みを試す", broken: "let mut flaky = Flaky { store: &mut self.store, left: None };" },
            Keep { path: SIM_RS, needle: "let result = map_observed(&mut self.k, &mut self.shadow, process, range, map_source, prot);", why: "map を通知つきで呼ぶ", broken: "let result = self.k.map(process, range, map_source, prot);" },
            Keep { path: SIM_RS, needle: "match unmap_observed(&mut self.k, &mut self.shadow, process, range) {", why: "unmap を通知つきで呼ぶ", broken: "match self.k.unmap(process, range) {" },
            Keep { path: SIM_RS, needle: "match protect_observed(&mut self.k, &mut self.shadow, process, range, prot) {", why: "protect を通知つきで呼ぶ", broken: "match self.k.protect(process, range, prot) {" },
            Keep { path: SIM_RS, needle: "        if seen != *model {", why: "通知から組み立てたアドレス空間をモデルと比べる", broken: "        if false {" },
            Keep { path: SIM_RS, needle: "if fault.addr() == GuestAddr::new(addr) && fault.kind() == kind =>", why: "アクセスの失敗の位置と種類をモデルと比べる", broken: "if fault.kind() == kind =>" },
            Keep { path: SIM_RS, needle: "} else if read.get(at).copied() != Some(self.content.byte(*backing, *offset)) {", why: "読んだ内容をモデルと比べる", broken: "} else if false {" },
            Keep { path: SIM_RS, needle: "kome::SpaceObserver::cleared(&mut self.shadow, process);", why: "プロセスの終了を通知の受け取り手へ伝える", broken: "" },
        ],
        breaks: &[
            |repo| repo.append(BACKEND_RS, "fn peek(kernel: &Kernel) -> u64 { kernel.backing_bytes }"),
            |repo| repo.edit(BACKEND_RS, |text| text.replace("|shown_in, range| code.invalidate(shown_in, range));", "|shown_in, range| if shown_in == process { code.invalidate(shown_in, range) });")),
            |repo| repo.edit(BACKEND_RS, |text| text.replace("    let mut failed = None;\n    walk(", "    written?;\n    let written: Result<(), AccessError> = Ok(());\n    let mut failed = None;\n    walk(")),
            |repo| repo.edit(SIM_TESTS_RS, |text| text.replace("\"code:dropped-alias\", ", "")),
            |repo| repo.edit(SIM_RS, |text| text.replacen("self.space_changed(process, false)?;", "self.check_space(process)?;", 1)),
        ],
    },
    Rule {
        id: "permit-returned",
        about: "実行権を値で受け取る公開の操作は、受理しなかった実行権を返す",
        check: permit_returned,
        keeps: &[
            Keep { path: INPUT_RS, needle: "Err(NotRelinquished::Rejected(permit, reason)) => Reply::Returned(permit, reason),", why: "Kernel::apply が、受理されなかった実行権を返答に入れて返していない", broken: "Err(NotRelinquished::Rejected(_, reason)) => return Err(reason)," },
        ],
        breaks: &[
            |repo| repo.edit("crates/kome/src/sched.rs", |text| text.replace(") -> Result<ThreadFate, NotRelinquished> {", ") -> Result<ThreadFate, Error> {")),
            |repo| repo.append(KERNEL_RS, "impl Kernel {\n    /// 捨てる。\n    pub fn discard(&mut self, permit: crate::sched::RunPermit) -> Result<(), Error> {\n        drop(permit);\n        Ok(())\n    }\n}"),
        ],
    },
    Rule {
        id: "stale-at-lookup",
        about: "古い ID と別インスタンスの ID の区別は、表を引く一か所 (id.rs) だけが行う",
        check: stale_at_lookup,
        keeps: &[],
        breaks: &[
            |repo| repo.append(KERNEL_RS, "fn looked(kernel: &Kernel, thread: ThreadId) -> Result<(), Error> {\n    kernel.threads.get(thread).map(|_| ()).ok_or(Error::Stale(crate::id::Tag::Thread))\n}"),
            |repo| repo.edit("crates/kome/src/id.rs", |text| text.replace(STALE_OR_FOREIGN, "Error::Stale(K::TAG)")),
            |repo| repo.edit("crates/kome/src/id.rs", |text| text.replace("slot.generation == id.generation && slot.issuer == id.issuer", "slot.generation == id.generation")),
        ],
    },
    Rule {
        id: "foreign-ids-tested",
        about: "ID を受け取る全ての操作と問い合わせの全ての引数に、別インスタンスの ID を渡す試験がある",
        check: foreign_ids_tested,
        keeps: &[],
        breaks: &[
            |repo| repo.edit(TESTS_RS, |text| text.replace("        Input::RetainObject(p, fo),\n", "")),
            |repo| repo.edit(TESTS_RS, |text| text.replace("        Input::PrepareIo(p, IoLinks::NONE.with_notify(fo)),\n", "")),
            |repo| repo.edit(TESTS_RS, |text| text.replace("    assert_eq!(k.io_outcome(fio), Err(Error::Foreign));\n", "")),
            |repo| repo.edit(INPUT_RS, |text| text.replace("    PollEffect,\n}", "    PollEffect,\n    /// 新しい操作。\n    Fresh(ObjectId),\n}")),
            |repo| repo.append(KERNEL_RS, "impl Kernel {\n    /// 新しい問い合わせ。\n    pub fn weight(&self, object: crate::id::ObjectId) -> Result<u64, Error> {\n        self.signal_count(object)\n    }\n}"),
            |repo| repo.edit(TESTS_RS, |text| text.replace(FOREIGN_REPLY, "let _ = own.k.apply(input);")),
        ],
    },
    Rule {
        id: "checked-insert",
        about: "試験の道具は、重複を拒否する前に既存の値を置き換えない",
        check: checked_insert,
        keeps: &[],
        breaks: &[
            |repo| repo.edit("crates/kome-testkit/src/host.rs", |text| text.replace("match self.memory.entry(backing) {", "match self.memory.insert(backing, vec![0; len]) {")),
            |repo| repo.edit(SIM_RS, |text| text.replace("self.blocked.insert(thread, targets);", "if self.blocked.insert(thread, targets).is_some() { return Err(String::new()); }")),
        ],
    },
    Rule {
        id: "advance-bounded",
        about: "一度の advance の仕事には目安があり、呼び出し側は残りが無くなるまで呼び直す",
        check: advance_bounded,
        keeps: &[
            Keep { path: TIMER_RS, needle: "pub fn advance(&mut self, to: Instant) -> Result<Advanced, Error> {", why: "advance が、進んだ時刻と残りの有無 (Advanced) を返していない", broken: "pub fn advance(&mut self, to: Instant) -> Result<u32, Error> {" },
            Keep { path: TIMER_RS, needle: "#[must_use = \"more() が真なら", why: "Advanced を捨てても警告が出ない (#[must_use] が無い)", broken: "#[doc = \"more() が真なら" },
            Keep { path: SIM_RS, needle: "        .with_advance_batch(NonZeroU32::MIN.saturating_add(1))\n", why: "決定的シミュレーションが、advance が区切って返る経路を通らない (目安を小さくする)", broken: "" },
            Keep { path: INPUT_RS, needle: "Input::Advance(to) => Reply::Advanced(self.advance(to)?),", why: "Kernel::apply が、advance の結果 (Advanced) をそのまま返答に入れていない", broken: "Input::Advance(to) => Reply::Count(self.advance(to)?.fired())," },
        ],
        breaks: &[
            |repo| repo.edit(TIMER_RS, |text| text.replace("if fired >= batch && last != Some(entry.at) {", "if false {")),
            |repo| repo.edit(TIMER_RS, |text| text.replace("if fired >= batch && last != Some(entry.at) {", "if fired >= batch {")),
            |repo| repo.edit("crates/kome-testkit/tests/headless.rs", |text| text.replace("while self.k.advance(to).unwrap().more() {}", "let _ = self.k.advance(to).unwrap();")),
            |repo| repo.edit(SIM_RS, |text| text.replace("if !advanced.more() {", "if true {")),
        ],
    },
    Rule {
        id: "docs-current",
        about: "文書が名指しする不変条件・規則・ファイルが実在し、台帳の全項目が責務の表にある",
        check: docs_current,
        keeps: &[],
        breaks: &[
            |repo| repo.edit(ARCHITECTURE_MD, |text| text.replace("| TIME-1 |", "| |")),
            |repo| repo.edit(ARCHITECTURE_MD, |text| text.replace("lint `no-globals`", "lint `no-statics`")),
            |repo| repo.edit(ARCHITECTURE_MD, |text| text.replace("`wait.rs`", "`waiting.rs`")),
            |repo| repo.edit(ARCHITECTURE_MD, |text| text.replace("| IO-1 |", "| IO-7 |")),
            |repo| repo.remove(MEMORY_MD),
        ],
    },
    Rule {
        id: "dup",
        about: "同じコードを二度書かない (意図した写しは docs/dup-known.tsv に理由つきで載せる)",
        check: dup_free,
        keeps: &[],
        breaks: &[
            // 関数をそのまま写す。
            |repo| repo.append(TIMER_RS, &format!("{DUP_FN}{}", DUP_FN.replace("dup_first", "dup_second"))),
            // 局所の名前だけを変えて写す。
            |repo| repo.append(TIMER_RS, &format!("{DUP_FN}{}", DUP_FN.replace("dup_first", "dup_second").replace("total", "sum").replace("entry", "each"))),
            // 続いた文を、別の関数の中へ写す。
            |repo| repo.append(TIMER_RS, &format!("{DUP_FN}{}", DUP_FN.replace("dup_first(", "dup_longer(third: u64, ").replace("    total\n}", "    total.saturating_add(third)\n}"))),
            // 同じ表を二つ持つ。
            |repo| repo.append(TIMER_RS, DUP_TABLE),
            // 同じ文字列を二度書く。
            |repo| repo.append(TIMER_RS, "fn dup_said() -> &'static str {\n    \"同じ説明の文を二か所に書くと、片方だけが直される日が来る。そうなる前に、名前を付けた定数にして一か所へ置く\"\n}\nfn dup_said_again() -> &'static str {\n    \"同じ説明の文を二か所に書くと、片方だけが直される日が来る。そうなる前に、名前を付けた定数にして一か所へ置く\"\n}"),
            // スクリプトの行を写す。
            |repo| repo.edit(TEST_SH, |text| format!("{text}\n{text}")),
            // CI の手順を、共通の Action を使わずにジョブへ書く。
            |repo| repo.edit(CI, |text| text.replace("      - uses: ./.github/actions/rust\n        with:\n          key: lint\n", "      - run: rustup show active-toolchain\n      - run: cargo --version\n      - run: rustc --version\n      - run: uname -a\n").replace("      - uses: ./.github/actions/rust\n        with:\n          key: loom\n", "      - run: rustup show active-toolchain\n      - run: cargo --version\n      - run: rustc --version\n      - run: uname -a\n")),
            // 一覧に、もう無い写しが残る。
            |repo| repo.append(dup::KNOWN, "exact\tcrates/kome/src/timer.rs#gone\tcrates/kome/src/io.rs#gone\t# 試験が実装と独立に持つ答えなので二つ持つ"),
            // 一覧に理由が無い。
            |repo| repo.edit(dup::KNOWN, |text| text.lines().map(|line| line.split("\t# ").next().unwrap_or(line)).collect::<Vec<_>>().join("\n")),
            // 一覧から行を消す。
            |repo| repo.edit(dup::KNOWN, |text| text.lines().filter(|line| line.starts_with('#')).collect::<Vec<_>>().join("\n")),
        ],
    },
    Rule {
        id: "ci-required",
        about: "CI は全ての検証を実行し、失敗を見逃さない",
        check: ci_required,
        keeps: &[],
        breaks: &[
            |repo| repo.edit(CI, |text| text.replace("scripts/ci/kani.sh", "true")),
            |repo| repo.edit(CI, |text| text.replace("--manifest-path checks/noalloc/Cargo.toml", "-p kome")),
            |repo| repo.edit(CI, |text| text.replace("scripts/ci/test.sh", "cargo test -p kome")),
            |repo| repo.edit(TEST_SH, |text| text.replace("cargo run -q --locked -p kome-lint -- ran \"$log\"\n", "")),
            |repo| repo.edit(TEST_SH, |text| text.replace("cargo test --workspace --locked", "cargo test -p kome --locked")),
            |repo| repo.edit(TEST_SH, |text| text.replace("set -euo pipefail", "set -eu")),
            |repo| repo.edit(CI, |text| text.replace("--target wasm32v1-none", "")),
            |repo| repo.edit(CI, |text| text.replacen("timeout-minutes:", "continue-on-error: true\n    timeout-minutes:", 1)),
            |repo| repo.edit(CI, |text| text.replace("cancel-in-progress: false", "cancel-in-progress: true")),
            |repo| repo.edit(CI, |text| text.replacen("actions/checkout@", "actions/checkout@v4 # ", 1)),
            |repo| repo.append("scripts/ci/tla.sh", "cargo fmt --all"),
            |repo| repo.append("scripts/ci/kani.sh", "echo \"$out\" | grep -q ok"),
            |repo| repo.append(".github/workflows/extra.yml", "jobs:\n  a:\n    steps:\n      - run: npm install"),
            |repo| repo.edit(CI, |text| text.replace("run: scripts/ci/test.sh", "run: echo scripts/ci/test.sh")),
            |repo| repo.edit(CI, |text| text.replace("run: cargo test --locked --manifest-path", "run: echo cargo test --locked --manifest-path")),
            |repo| repo.edit(CI, |text| text.replace("      - name: kome-lint\n", "      - name: kome-lint\n        if: false\n")),
            |repo| repo.edit(CI, |text| text.replace("      - name: kani\n        run:", "      - name: kani\n        shell: echo {0}\n        run:")),
            |repo| repo.edit(CI, |text| text.replace("    name: tla\n", "    name: tla\n    if: github.event_name == 'schedule'\n")),
            |repo| repo.append(CI, "defaults:\n  run:\n    shell: echo {0}"),
            |repo| repo.edit(CI, |text| text.replace("  push:\n    branches: [main]\n", "")),
            |repo| repo.edit(CI, |text| text.replace("      - name: miri\n        run: cargo miri test -p kome --locked --lib", "      - name: miri\n        env:\n          CARGO: echo\n        run: cargo miri test -p kome --locked --lib")),
            |repo| repo.edit(CI, |text| text.replace("  kani:\n    name: kani\n", "  kani:\n    name: kani\n    env:\n      PATH: /nowhere\n")),
            |repo| repo.edit(CI, |text| {
                let moved = text.replace("      - name: kani\n        run: scripts/ci/kani.sh\n", "");
                moved.replace("      - name: tla\n        run: scripts/ci/tla.sh", "      - name: tla\n        run: scripts/ci/tla.sh\n      - name: kani\n        run: scripts/ci/kani.sh")
            }),
            |repo| repo.edit(CI, |text| text.replace("runs-on: ubuntu-latest\n    timeout-minutes: 30", "runs-on: self-hosted\n    timeout-minutes: 30")),
        ],
    },
];

fn kernel_files(repo: &Repo) -> impl Iterator<Item = (&str, &str)> {
    repo.under(KOME_SRC, ".rs").filter(|(path, _)| !path.ends_with("/tests.rs") && !path.ends_with("/conform.rs") && !path.ends_with("/proofs.rs"))
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
        // fuzz と checks/noalloc はワークスペースの外にある。libFuzzer の入口と、確保を数える
        // アロケータは unsafe を要るので、unsafe を禁止するワークスペースの設定を継承できない。
        let outside = ["fuzz/Cargo.toml", "checks/noalloc/Cargo.toml"];
        let member = path != "Cargo.toml" && path != KOME_MANIFEST && !outside.contains(&path);
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
    let files = repo.under("crates/", ".rs").chain(repo.under("fuzz/", ".rs")).chain(repo.under("checks/", ".rs"));
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
    let mut unwinds: Vec<(String, String)> = Vec::new();
    let mut pending: Option<String> = None;
    for line in repo.text(proofs_rs).lines() {
        if line == "#[kani::proof]" {
            pending = Some("-".to_owned());
        } else if let Some(count) = line.strip_prefix("#[kani::unwind(").and_then(|rest| rest.strip_suffix(")]")) {
            pending = pending.map(|_| count.to_owned());
        } else if let Some(name) = line.strip_prefix("fn ").and_then(|rest| rest.split('(').next())
            && let Some(unwind) = pending.take()
        {
            unwinds.push((name.to_owned(), unwind));
        }
    }
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
        // 証明とモデルが何を示し、何を示していないかを、項目ごとに書く。
        let cited = strings(entry, "proofs");
        if cited.is_empty() != field("proved").trim().is_empty() {
            bad("proved は、proofs があるときにだけ書く (何を示したか、前提、示していないこと)".to_owned());
        }
        let bounded = cited.iter().any(|proof| unwinds.iter().any(|(name, unwind)| name == proof && unwind != "-"));
        if bounded && !(field("proved").contains("小さな上限") && field("proved").contains("前提:")) {
            bad("ループ展開の上限を持つ証明を挙げているのに、proved に「小さな上限」と「前提:」が無い".to_owned());
        }
        let modules: BTreeSet<String> =
            strings(entry, "models").iter().filter_map(|model| model.split_once(':')).map(|(module, _)| module.to_owned()).collect();
        for key in ["modelled", "tied", "tested"] {
            if modules.is_empty() != field(key).trim().is_empty() {
                bad(format!("{key} は、models があるときにだけ書く (モデルが調べること、生成した表が結ぶ範囲、試験が見る範囲)"));
            }
        }
        for (module, table, _) in GENERATED {
            if modules.contains(module) && !field("tied").contains(table) {
                bad(format!("tied が、{module} と実装を結ぶ表 {table} に触れていない"));
            }
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
    // 証明ごとの検証条件と範囲 (docs/proofs.tsv)。ループ展開の上限は proofs.rs の属性と同じ値を書く。
    let bounds: Vec<(&str, &str, &str)> = repo
        .text(PROOFS_TSV)
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .filter_map(|line| {
            let mut cells = line.split('\t');
            Some((cells.next()?, cells.next()?, cells.next()?))
        })
        .collect();
    for (name, unwind) in &unwinds {
        let row = bounds.iter().find(|(listed, _, _)| listed == name);
        match row {
            None => report.add(PROOFS_TSV, 1, format!("証明 {name} の検証条件と範囲が無い")),
            Some((_, listed, _)) if listed != unwind => {
                report.add_at(repo, PROOFS_TSV, name, format!("{name}: ループ展開の上限は {unwind} (表では {listed})"));
            }
            Some((_, _, range)) if range.trim().is_empty() => {
                report.add_at(repo, PROOFS_TSV, name, format!("{name}: 範囲が空"));
            }
            Some((_, _, range)) if unwind != "-" && !range.starts_with("小さな上限での証明。前提: ") => {
                report.add_at(repo, PROOFS_TSV, name, format!("{name}: ループ展開の上限を持つ証明の範囲は「小さな上限での証明。前提: 」で始める"));
            }
            Some(_) => {}
        }
    }
    for (listed, _, _) in &bounds {
        if !unwinds.iter().any(|(name, _)| name == listed) {
            report.add_at(repo, PROOFS_TSV, listed, format!("証明 {listed} は proofs.rs に無い"));
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

/// モデルの `VARIABLES` の名前。
fn model_variables(text: &str) -> Vec<&str> {
    let Some((_, rest)) = text.split_once("\nVARIABLES\n") else { return Vec::new() };
    rest.lines()
        .take_while(|line| !line.trim().is_empty())
        .filter_map(|line| line.trim().split(|c: char| !(c.is_alphanumeric() || c == '_')).next())
        .filter(|name| !name.is_empty())
        .collect()
}

fn tla_conform(repo: &Repo, report: &mut Report) {
    let (conform, doc, script) = (repo.text(CONFORM_RS), repo.text(CONFORM_MD), repo.text(TLA_SH));
    let expect = expectations(repo);
    let modules: BTreeSet<&str> = expect.iter().filter(|(_, _, want)| *want == "ok").map(|(_, module, _)| *module).collect();
    for module in modules {
        if !conform.contains(&format!("const NAME: &'static str = \"{module}\";")) {
            report.add(CONFORM_RS, 1, format!("{module} を実装と突き合わせる試験が無い"));
        }
        let section = doc.split_once(&format!("\n### {module} ")).map_or("", |(_, rest)| rest.split("\n### ").next().unwrap_or(""));
        if section.is_empty() {
            report.add(CONFORM_MD, 1, format!("{module} の、変数と実装の対応の節が無い"));
        }
        let model = format!("tla/{module}.tla");
        let variables = model_variables(repo.text(&model));
        if variables.is_empty() {
            report.add(&model, 1, "VARIABLES を読めない");
        }
        for name in variables {
            let listed = section.contains(&format!("`{name}`")) || section.contains(&format!("`{name}["));
            if !listed {
                report.add_at(repo, &model, name, format!("変数 {name} と実装の値の対応を {CONFORM_MD} に書く"));
            }
            if !conform.contains(&format!("format!(\"{name} = ")) && !conform.contains(&format!("\"{name} = ")) {
                report.add_at(repo, &model, name, format!("変数 {name} を、実装の状態から作っていない ({CONFORM_RS})"));
            }
        }
    }
    let kept = [
        (CONFORM_RS, conform, "    assert!(explore::<D>(&fewer).is_err(), ", "モデルから遷移を抜くと一致しなくなることを確かめる"),
        (CONFORM_RS, conform, "    assert!(never_taken(&more, &taken).is_err(), ", "モデルに遷移を足すと一致しなくなることを確かめる"),
        (TLA_SH, script, "out=\"$(KOME_TLA_DUMPS=\"$graphs\" cargo test --release --locked -p kome --lib conform:: -- --nocapture 2>&1)\" || {", "突き合わせの試験を全て走らせる"),
        (TLA_SH, script, "    if line=\"$(echo \"$out\" | grep -o \"conform $module: .*\")\"; then", "モデルごとに、突き合わせが走ったことを見る"),
        (TLA_SH, script, "        failed=1\n    fi\ndone\nexit \"$failed\"\n", "突き合わせが走らなければ失敗にする"),
    ];
    for (path, text, needed, what) in kept {
        if !text.contains(needed) {
            report.add(path, 1, format!("モデルと実装の突き合わせが崩れている: {what}"));
        }
    }
}

/// モデル、実装から生成した表、その表が定める関係。
const GENERATED: [(&str, &str, &str); 6] = [
    ("IoLifecycle", "IoTable", "IoStep"),
    ("WaitRace", "WaitTable", "WaitDecide"),
    ("ProcessExit", "IoTable", "IoStep"),
    ("ProcessExit", "ExitTable", "ExitThread"),
    ("ProcessExit", "ExitTable", "ExitFinish"),
    ("ProcessExit", "ExitTable", "ExitComplete"),
];

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
    for (model, table, relation) in GENERATED {
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
    let input = INPUT_RS;
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
            // `apply` は Input の入口で、Input の変種ではない。出力の取り出し (`poll_effect`) は、
            // 後の操作の結果を変えるので、入力として記録する。
            if name != "apply" && !variants.contains(&camel(name)) {
                report.add(path, *line, format!("操作 {name} に対応する Input::{} が無い", camel(name)));
            }
            if name != "apply" && !calls.contains(name) {
                report.add(path, *line, format!("操作 {name} を決定的シミュレーション ({sim}) が呼んでいない"));
            }
        }
    }
    // 写しと復元は状態を変えない操作 (`&self`) なので上の一覧に入らない。名指しで求める。
    for name in ["snapshot", "restore", "peek_effect"] {
        if !calls.contains(name) {
            report.add(sim, 1, format!("決定的シミュレーションが {name} を呼んでいない"));
        }
    }
    for variant in &variants {
        if !operations.contains(variant) {
            report.add_at(repo, input, &format!("{variant}("), format!("Input::{variant} に対応する Kernel の操作が無い"));
        }
    }
}

/// 確保しないと約束した操作 (docs/memory.md の二つ目の表)。
/// 容量の中でだけ追加する呼び出しの、入れ物の名前 (`self.space.limbo.push_within(..)` なら `limbo`)。
fn within_receivers(text: &str) -> Vec<(String, usize)> {
    let mut found = Vec::new();
    for (at, line) in text.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for call in [".push_within(", ".push_back_within(", ".extend_within("] {
            for (place, _) in line.match_indices(call) {
                let before = line.get(..place).unwrap_or("");
                let name: String = before.chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                found.push((name.chars().rev().collect(), at.saturating_add(1)));
            }
        }
    }
    found
}

fn capacity_audited(repo: &Repo, report: &mut Report) {
    let audit_rs = "crates/kome/src/audit.rs";
    let rows: Vec<Vec<&str>> = repo
        .text(CAPACITY)
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| line.split('\t').collect())
        .collect();
    if rows.is_empty() {
        report.add(CAPACITY, 1, "容量の表が無い、または空である");
    }
    let mut used = BTreeSet::new();
    for (path, text) in kernel_files(repo).filter(|(path, _)| !path.ends_with("/store.rs")) {
        for (name, line) in within_receivers(text) {
            let Some(row) = rows.iter().find(|row| row.first() == Some(&name.as_str())) else {
                report.add(path, line, format!("`{name}` へ確保なしで追加しているが、{CAPACITY} に容量を確保する操作が書かれていない"));
                continue;
            };
            used.insert(name.clone());
            if row.get(3) == Some(&"同じ操作") && !text.contains(&format!("{name}.reserve(")) {
                report.add(path, line, format!("`{name}` は「同じ操作」が確保すると書かれているが、このファイルに `{name}.reserve(` が無い"));
            }
        }
    }
    for row in &rows {
        let [name, reserves, uses, check] = row.as_slice() else {
            report.add_at(repo, CAPACITY, row.first().copied().unwrap_or(""), "列は 4 つ (入れ物、確保する操作、使う操作、確かめ方)");
            continue;
        };
        if reserves.trim().is_empty() || uses.trim().is_empty() {
            report.add_at(repo, CAPACITY, name, "確保する操作と使う操作を書く");
        }
        if !used.contains(*name) {
            report.add_at(repo, CAPACITY, name, format!("`{name}` へ確保なしで追加する呼び出しが kome に無い (表が古い)"));
        }
        if *check != "同じ操作" && !repo.text(audit_rs).contains(&format!("\"{check}\"")) {
            report.add_at(repo, CAPACITY, name, format!("audit.rs が「{check}」を報告しない (容量を検査していない)"));
        }
    }
}

fn checked_insert(repo: &Repo, report: &mut Report) {
    for (path, text) in repo.under("crates/kome-testkit/src/", ".rs") {
        for (at, line) in text.lines().enumerate() {
            let code = line.trim();
            // 対応表の insert は値を置き換えて古い値を返す。その返り値で重複を判定すると、
            // 拒否するときには既に置き換わっている。
            let judged = code.starts_with("match ") || code.contains(").is_some()") || code.contains(").is_none()");
            if !code.starts_with("//") && code.contains(".insert(") && code.contains(", ") && judged {
                report.add(path, at.saturating_add(1), "置き換えてから重複を判定している。entry() か contains_key() で先に確かめる");
            }
        }
    }
}

fn stale_at_lookup(repo: &Repo, report: &mut Report) {
    let id_rs = "crates/kome/src/id.rs";
    // error.rs は変種を定義するだけで、作らない。
    for (path, text) in kernel_files(repo).filter(|(path, _)| *path != id_rs && !path.ends_with("/error.rs")) {
        let Some(found) = scanned(report, path, text, true) else { continue };
        for (name, line) in &found.idents {
            if name == "Stale" {
                report.add(path, *line, "`Error::Stale` を直接作っている。公開の操作が受け取った ID は Arena::find / find_mut / take で引く (別インスタンスの ID を Foreign にする)");
            }
        }
    }
    for needed in [
        STALE_OR_FOREIGN,
        "(slot.generation == id.generation && slot.issuer == id.issuer).then_some(slot)",
        "if slot.generation != id.generation || slot.issuer != id.issuer {",
        "if slot.generation != id.generation || slot.issuer != id.issuer || !matches!(slot.cell, Entry::Live(_)) {",
    ] {
        if !repo.text(id_rs).contains(needed) {
            report.add(id_rs, 1, format!("表を引く処理が ID の発行元を照合していない (`{needed}`)"));
        }
    }
}

/// 型の中の ID の数。`IoLinks` は通知先を二つ、`MapSource` は実体を一つ、`Completion` は I/O を一つ持つ。
fn ids_in(ty: &syn::Type) -> usize {
    let mut names = Vec::new();
    crate::scan::idents_of(ty, &mut names);
    names
        .iter()
        .map(|name| match name.as_str() {
            "IoLinks" => 2,
            "MapSource" | "Completion" => 1,
            // 実行インスタンスと worker の識別は、kome が発行する資源の ID ではない。
            "InstanceId" | "WorkerId" => 0,
            other => usize::from(other.ends_with("Id")),
        })
        .sum()
}

fn foreign_ids_tested(repo: &Repo, report: &mut Report) {
    let input = INPUT_RS;
    let test = "fn a_foreign_id_is_refused_in_every_position()";
    let body = repo.text(TESTS_RS).split_once(test).and_then(|(_, rest)| rest.split_once("\n}\n")).map_or("", |(body, _)| body);
    if body.is_empty() {
        report.add(TESTS_RS, 1, format!("試験 `{test}` が無い"));
    }
    for needed in [FOREIGN_REPLY, "assert_eq!(own.k, before, \"{at}: {shown} が状態を変えた\");"] {
        if !body.contains(needed) {
            report.add_at(repo, TESTS_RS, test, format!("試験が、拒否の理由と状態が変わらないことを確かめていない (`{needed}`)"));
        }
    }
    match syn::parse_file(repo.text(input)) {
        Ok(file) => {
            for item in &file.items {
                let syn::Item::Enum(found) = item else { continue };
                if found.ident != "Input" {
                    continue;
                }
                for variant in &found.variants {
                    let ids: usize = variant.fields.iter().map(|field| ids_in(&field.ty)).sum();
                    let tried = body.matches(&format!("Input::{}(", variant.ident)).count();
                    if tried < ids {
                        report.add_at(repo, input, &format!("{}(", variant.ident), format!("Input::{} は ID を {ids} 個受け取るが、別インスタンスの ID を渡す試験は {tried} 個 ({TESTS_RS} の `{test}`)", variant.ident));
                    }
                }
            }
        }
        Err(error) => report.add(input, 1, format!("構文を読めない: {error}")),
    }
    for (path, text) in kernel_files(repo) {
        let Ok(file) = syn::parse_file(text) else { continue };
        for item in &file.items {
            let syn::Item::Impl(block) = item else { continue };
            let is_kernel = matches!(&*block.self_ty, syn::Type::Path(path) if path.path.is_ident("Kernel"));
            for member in &block.items {
                let syn::ImplItem::Fn(method) = member else { continue };
                let reads = method.sig.receiver().is_some_and(|receiver| receiver.mutability.is_none());
                let ids: usize = method.sig.inputs.iter().map(|input| match input {
                    syn::FnArg::Typed(typed) => ids_in(&typed.ty),
                    syn::FnArg::Receiver(_) => 0,
                }).sum();
                let public = matches!(method.vis, syn::Visibility::Public(_));
                if is_kernel && public && reads && ids > 0 && !body.contains(&format!("k.{}(", method.sig.ident)) {
                    let line = method.sig.ident.span().start().line;
                    report.add(path, line, format!("問い合わせ `{}` に、別インスタンスの ID を渡す試験が無い ({TESTS_RS} の `{test}`)", method.sig.ident));
                }
            }
        }
    }
}

fn permit_returned(repo: &Repo, report: &mut Report) {
    for (path, text) in kernel_files(repo) {
        let Ok(file) = syn::parse_file(text) else {
            report.add(path, 1, "構文を読めない");
            continue;
        };
        for item in &file.items {
            let syn::Item::Impl(block) = item else { continue };
            for member in &block.items {
                let syn::ImplItem::Fn(method) = member else { continue };
                if !matches!(method.vis, syn::Visibility::Public(_)) {
                    continue;
                }
                let takes_permit = method.sig.inputs.iter().any(|input| match input {
                    syn::FnArg::Typed(typed) => {
                        matches!(&*typed.ty, syn::Type::Path(path) if path.path.segments.last().is_some_and(|last| last.ident == "RunPermit"))
                    }
                    syn::FnArg::Receiver(_) => false,
                });
                let mut output = Vec::new();
                if let syn::ReturnType::Type(_, ty) = &method.sig.output {
                    crate::scan::idents_of(ty, &mut output);
                }
                if takes_permit && !output.iter().any(|name| name == "NotRelinquished") {
                    let line = method.sig.ident.span().start().line;
                    report.add(path, line, format!("`{}` は実行権を値で受け取るが、拒否したときに返さない (失敗の型を NotRelinquished にする)", method.sig.ident));
                }
            }
        }
    }
}

/// 関数の本体の行の範囲を集める。
#[derive(Default)]
struct Bodies(Vec<(String, usize, usize)>);

impl Bodies {
    fn push(&mut self, name: &syn::Ident, block: &syn::Block) {
        let span = block.brace_token.span;
        self.0.push((name.to_string(), span.open().start().line, span.close().end().line));
    }
}

impl<'ast> syn::visit::Visit<'ast> for Bodies {
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.push(&node.sig.ident, &node.block);
        syn::visit::visit_item_fn(self, node);
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.push(&node.sig.ident, &node.block);
        syn::visit::visit_impl_item_fn(self, node);
    }
}

fn advance_bounded(repo: &Repo, report: &mut Report) {
    let timer = repo.text(TIMER_RS);
    // 区切るのは、目安に達していて、次の期限の時刻が直前に処理した期限と違うときだけ。
    if !timer.contains("if fired >= batch && last != Some(entry.at) {") || !timer.contains("let batch = self.limits.advance_batch().get();") {
        report.add_at(repo, TIMER_RS, "pub fn advance(", "advance が、目安の数を処理したところ (同じ時刻の期限を処理し終えたところ) で返っていない");
    }
    // kome の外で advance を呼ぶ関数は、残りがあるかを見る。
    for (path, text) in repo.under("", ".rs") {
        if path.starts_with(KOME_SRC) || path.starts_with("crates/kome-lint/") {
            continue;
        }
        let Ok(file) = syn::parse_file(text) else { continue };
        let mut bodies = Bodies::default();
        syn::visit::Visit::visit_file(&mut bodies, &file);
        let lines: Vec<&str> = text.lines().collect();
        for (name, first, last) in &bodies.0 {
            let body = lines.get(first.saturating_sub(1)..*last).unwrap_or(&[]);
            let calls = body.iter().any(|line| line.contains(".advance("));
            if calls && !body.iter().any(|line| line.contains(".more()")) {
                report.add(path, *first, format!("`{name}` は advance を呼ぶが、残りがあるか (Advanced::more) を見ていない"));
            }
        }
    }
}

fn independent_model(repo: &Repo, report: &mut Report) {
    let Some(found) = scanned(report, PAGES_RS, repo.text(PAGES_RS), true) else { return };
    let mut after_kome = false;
    for (name, line) in &found.idents {
        // `kome::` の次に来てよい名前は BackingId だけ。kome の型と判定の名前は、どこにも書かない。
        let borrowed = after_kome && name != "BackingId";
        let judged = matches!(name.as_str(), "Prot" | "Access" | "allows" | "covers" | "Kernel" | "translate");
        if borrowed || judged {
            report.add(PAGES_RS, *line, format!("モデルが kome の `{name}` を使っている。モデルは自分の表現で判定する"));
        }
        after_kome = name == "kome";
    }
    for needed in ["self.check_merged(process, first, count)?;", "&& model.continues(page, PAGE)"] {
        if !repo.text(SIM_RS).contains(needed) {
            report.add(SIM_RS, 1, format!("決定的シミュレーションが、権限の変更の後に項目がまとまったことをモデルと比べていない (`{needed}`)"));
        }
    }
}

fn trace_complete(repo: &Repo, report: &mut Report) {
    let input = INPUT_RS;
    let variants = scanned(report, input, repo.text(input), true).map(|found| found.variants).unwrap_or_default();
    let (lib, words, tests) = (repo.text(TRACE_RS), repo.text(TRACE_WORDS_RS), repo.text(TRACE_TESTS_RS));
    let mut inputs = 0usize;
    for (of, name) in &variants {
        if of == "Input" {
            inputs = inputs.saturating_add(1);
            // 書く側は Input の全ての項目を並べるので、足りなければコンパイルできない。読む側と試験は名前で分ける。
            for (path, text, needed, what) in [
                (TRACE_WORDS_RS, words, format!("        \"{name}\" => {{"), "記録を読む側 (apply_input) に無い"),
                (TRACE_WORDS_RS, words, format!("Input::{name}"), "記録を書く側 (write_input) に無い"),
                (TRACE_TESTS_RS, tests, format!("\"{name}\","), "全ての操作を記録して再生する試験の一覧に無い"),
                (TRACE_TESTS_RS, tests, format!("Input::{name}"), "試験が作る操作に無い"),
            ] {
                if !text.contains(&needed) {
                    report.add(path, 1, format!("Input::{name} が、{what}"));
                }
            }
        }
        // 返答が返した ID と実行権は、番号の表に入れる。新しい返答は、入れるものが無いことも書く。
        if of == "Reply" && lib.matches(&format!("Reply::{name}")).count() < 2 {
            report.add(TRACE_RS, 1, format!("Reply::{name} を、番号の表 (learn) と実行権の保持 (keep) の両方で扱う"));
        }
    }
    if !tests.contains(&format!("const KINDS: u64 = {inputs};")) {
        report.add(TRACE_TESTS_RS, 1, format!("試験が作る操作の種類の数を、Input の項目の数 ({inputs}) に合わせる"));
    }
    let kept = [
        (TRACE_RS, "        if recorded != replayed {\n            return Err(Stopped::Mismatch {", "返答が記録と違ったら、その行で止まる"),
        (TRACE_RS, "            return Err(Stopped::NotReplayable { line: at, input: input.to_owned() });", "再生できない入力で止まる"),
        (TRACE_TESTS_RS, "fn the_saved_recording_still_replays() {", "保存してある記録を再生する試験"),
        (TRACE_TESTS_RS, "fn any_damage_to_the_text_stops_the_replay(", "記録を壊すと再生が止まる試験"),
        (TRACE_TESTS_RS, "fn a_missing_input_stops_the_replay(", "入力を抜くと再生が止まる試験"),
        (TRACE_TESTS_RS, "fn a_changed_reply_is_a_mismatch_at_that_line(", "返答を変えると不一致になる試験"),
    ];
    for (path, needed, what) in kept {
        if !repo.text(path).contains(needed) {
            report.add(path, 1, format!("記録と再生の約束が崩れている: {what}"));
        }
    }
    if !repo.has("crates/kome-trace/tests/golden/basic.trace") {
        report.add(TRACE_TESTS_RS, 1, "保存してある記録 (tests/golden/basic.trace) が無い");
    }
    // 記録の形式は、kome と標準ライブラリだけで読み書きする。
    let manifest = repo.text("crates/kome-trace/Cargo.toml");
    let deps = manifest.split_once("[dependencies]\n").map_or("", |(_, rest)| rest.split("\n[").next().unwrap_or(""));
    if deps.lines().filter(|line| !line.trim().is_empty()).collect::<Vec<_>>() != ["kome = { path = \"../kome\" }"] {
        report.add("crates/kome-trace/Cargo.toml", 1, "kome-trace が依存するのは kome だけにする");
    }
}

fn run_queue_kept(repo: &Repo, report: &mut Report) {
    // 列とその中の位置を書くのは runq.rs だけ。ほかの場所が書くと、位置と席がずれる。
    let writes = [
        ("self.ready.push", "列へ入れる"),
        ("kernel.ready.push", "列へ入れる"),
        ("self.ready.pop", "列から取り出す"),
        ("self.ready.swap", "列を並べ替える"),
        ("self.ready.remove", "列から取り出す"),
        ("self.ready.get_mut", "列の席を書き換える"),
        (".queued = ", "列の中の位置を書く"),
        (".turn = ", "実行待ちになった順番を書く"),
    ];
    // 列を全て見るのは、検査 (audit.rs) だけ。貸す相手を選ぶために全て見ると、スレッドの数に比例して遅くなる。
    let scans = ["self.ready.iter()", "self.ready.position(", "self.ready.as_slice()"];
    for (path, text) in repo.under(KOME_SRC, ".rs") {
        if path == RUNQ_RS || path == TESTS_RS {
            continue;
        }
        for (at, line) in text.lines().enumerate() {
            for (write, what) in writes {
                if line.contains(write) {
                    report.add(path, at.saturating_add(1), format!("{what}のは {RUNQ_RS} だけにする"));
                }
            }
            if path != "crates/kome/src/audit.rs" && scans.iter().any(|scan| line.contains(scan)) {
                report.add(path, at.saturating_add(1), "実行待ちの列を全て見ない。貸す相手は列の先頭、名指しはスレッドが持つ位置で引く");
            }
        }
    }
    let kept = [
        (SCHED_RS, "        let at = 0;\n        if self.ready.is_empty() {\n            return Ok(None);\n        }\n        self.lend(worker, at).map(Some)", "grant は列の先頭を貸す"),
        (SCHED_RS, "                self.flush_limbo(process)?;\n                self.let_in(process)?;\n", "古い世代の実行が無くなったら、外していたスレッドを列へ戻す"),
        (MEM_RS, "        if barred {\n            // 残っている間は、このプロセスのスレッドへ実行権を貸さない (台帳 MEM-3)。\n            self.hold_back(process)?;\n", "古い世代の実行が残る間、そのプロセスのスレッドを列から外す"),
        (RUNQ_RS, "pub(crate) const fn lent_before(a: (Priority, u64), b: (Priority, u64)) -> bool {", "貸す順序は整数だけの関数で決め、Kani が全ての値で示す"),
        ("crates/kome/src/proofs.rs", "fn the_lending_order_is_a_strict_total_order() {", "貸す順序が必ず付くことを Kani が示す"),
        ("crates/kome/src/audit.rs", "parent.is_none_or(|parent| !waiting.ahead_of(parent)),", "検査が、列の先頭が最も先に貸す席であることを確かめる"),
    ];
    for (path, needed, what) in kept {
        if !repo.text(path).contains(needed) {
            report.add(path, 1, format!("実行待ちの列の約束が崩れている: {what}"));
        }
    }
    // 並べ替えの繰り返しは、木の深さで必ず止まる形にする。
    let runq = repo.text(RUNQ_RS);
    if runq.matches("        for _ in 0..usize::BITS {").count() != 2 || runq.contains("loop {") {
        report.add(RUNQ_RS, 1, "席を上下へ動かす繰り返しは、回数に上限のある形 (`for _ in 0..usize::BITS`) で書く");
    }
}

fn host_single_writer(repo: &Repo, report: &mut Report) {
    let host = repo.text(HOST_RS);
    // 同期の部品は sync.rs だけが選ぶ。ほかの場所が標準のものを直接使うと、Loom から見えない。
    let sync_rs = "crates/kome-host-std/src/sync.rs";
    for (path, text) in repo.under("crates/kome-host-std/src/", ".rs") {
        if path == sync_rs {
            continue;
        }
        for (at, line) in text.lines().enumerate() {
            if line.contains("std::sync") || line.contains("std::thread") || line.contains("loom::") {
                report.add(path, at.saturating_add(1), format!("同期の部品は {sync_rs} を通して使う (Loom が差し替えられるように)"));
            }
        }
    }
    let sync = repo.text(sync_rs);
    for needed in [
        "#[cfg(loom)]\npub(crate) use loom::sync::{Arc, Condvar, Mutex, MutexGuard, atomic::AtomicU64, atomic::Ordering};",
        "#[cfg(not(loom))]\npub(crate) use std::sync::{Arc, Condvar, Mutex, MutexGuard, atomic::AtomicU64, atomic::Ordering};",
    ] {
        if !sync.contains(needed) {
            report.add(sync_rs, 1, "同期の部品を、Loom のものと標準のものの同じ組で差し替える");
        }
    }
    let kept = [
        ("            drop(guard);\n            let stopped = executor.run(&permit, loaded.context());\n            guard = self.lock()?;\n", "ゲストは錠を放してから実行し、戻ったら取り直す"),
        ("                    break start_run(&mut state.kernel, permit, &mut state.contexts)?;", "文脈は start_run で取り出す"),
        ("            let ran = finish_run(&mut state.kernel, permit, loaded, stopped, &mut state.contexts)?;", "実行権は finish_run で返す"),
    ];
    for (needed, what) in kept {
        if !host.contains(needed) {
            report.add(HOST_RS, 1, format!("参照ホストの約束が崩れている: {what}"));
        }
    }
    if host.matches("executor.run(").count() != 1 {
        report.add_at(repo, HOST_RS, "executor.run(", "ゲストを実行する場所は一つだけにする");
    }
    // 実行権の貸し借りは kome の入口 (start_run / finish_run) に任せ、自分で返さない。
    if let Some(at) = host.lines().position(|line| line.contains(".relinquish(")) {
        report.add(HOST_RS, at.saturating_add(1), "参照ホストが実行権を自分で返している。finish_run を使う");
    }
    let loom = repo.text(LOOM_RS);
    for needed in [
        "    explore(1, 1, 1, None);",
        "    explore(2, 1, 1, Some(7));",
        "    explore(2, 2, 0, Some(7));",
        "        host.with(|state| state.kernel.unsuspend_thread(guest)).unwrap().unwrap();",
        "        let host = support::finished_host(2, || Instant::from_nanos(0));",
        "        support::interrupt_names_one_run(|| Instant::from_nanos(0), thread::yield_now, |work| {",
        "        host.wait_until(|state| state.personality.process_exited).unwrap();",
        "            assert_eq!(worker.join().unwrap(), Ok(()));",
        "        support::check(&host, &made, yields);\n",
    ] {
        if !loom.contains(needed) {
            report.add(LOOM_RS, 1, format!("Loom の検査が足りない (`{}`)", needed.trim()));
        }
    }
    let script = repo.text(LOOM_SH);
    for (needed, why) in [
        ("RUSTFLAGS=\"--cfg loom\" cargo test -p kome-host-std --test loom --release --locked 2>&1 | tee \"$log\"\n", "進み方を試す"),
        ("if RUSTFLAGS=\"--cfg loom --cfg loom_broken\" cargo test -p kome-host-std --test loom --release --locked >\"$log\" 2>&1; then\n", "通知を抜いた誤りが見つかることを確かめる"),
        ("if ! grep -q \"deadlock\" \"$log\"; then\n", "見つかった理由が行き詰まりであることを確かめる"),
    ] {
        if !script.contains(needed) {
            report.add(LOOM_SH, 1, format!("Loom のスクリプトが足りない: {why}"));
        }
    }
}

fn run_identity(repo: &Repo, report: &mut Report) {
    let kept = [
        (SIM_TESTS_RS, "        \"run:again-after-interrupt\",\n        \"run:other-thread-after-interrupt\",\n", "決定的シミュレーション: 求めが残った worker への次の貸し出しに到達する"),
    ];
    for (path, needed, what) in kept {
        if !repo.text(path).contains(needed) {
            report.add(path, 1, format!("実行の識別の約束が崩れている: {what} (`{}`)", needed.trim()));
        }
    }
}

fn bench_listed(repo: &Repo, report: &mut Report) {
    let bench = repo.text(BENCH_RS);
    let doc = repo.text(BENCH_MD);
    // 測定をまとめた関数 (`fn 名前(bench: &Bench)`) は、全て main から呼ぶ。
    for (at, line) in bench.lines().enumerate() {
        let Some(name) = line.strip_prefix("fn ").and_then(|rest| rest.strip_suffix("(bench: &Bench) {")) else { continue };
        if !bench.contains(&format!("    {name}(&bench);\n")) {
            report.add(BENCH_RS, at.saturating_add(1), format!("測定 `{name}` が main から呼ばれていない"));
        }
    }
    // 測定の名前 (かっこの前まで) は、文書の表に載せる。
    let mut names = 0usize;
    for (at, line) in bench.lines().enumerate() {
        let Some((_, rest)) = line.split_once("measure(\"").or_else(|| line.split_once("format!(\"")) else { continue };
        let name = rest.split(['"', '(']).next().unwrap_or("").trim();
        names = names.saturating_add(1);
        if name.is_empty() || !doc.lines().any(|row| row.starts_with(&format!("| {name}"))) {
            report.add(BENCH_RS, at.saturating_add(1), format!("測定「{name}」が {BENCH_MD} の表に無い"));
        }
    }
    if names == 0 {
        report.add(BENCH_RS, 1, "測定が一つも無い");
    }
    if !repo.text("crates/kome-bench/Cargo.toml").contains("[[bench]]\nname = \"kernel\"\nharness = false") {
        report.add("crates/kome-bench/Cargo.toml", 1, "測定は自前の計測器で動かす (`--smoke` を受け取れるように)");
    }
    // 遅くなっていないことの判定。記録の行は文書の表の操作で、表の操作は (錠を奪い合う測定を除いて) 全て記録にある。
    if !repo.has(BENCH_GATE) {
        report.add(BENCH_GATE, 1, "判定に使う記録が無い (`cargo bench -p kome-bench -- --baseline docs/bench-gate.tsv`)");
        return;
    }
    let gate = repo.text(BENCH_GATE);
    let recorded: Vec<&str> = gate.lines().filter(|line| !line.starts_with('#')).filter_map(|line| line.split_once('\t')).map(|(name, _)| name).collect();
    for (at, line) in gate.lines().enumerate() {
        let Some((name, _)) = line.split_once('\t').filter(|_| !line.starts_with('#')) else { continue };
        if !doc.contains(&format!("| {name} |")) {
            report.add(BENCH_GATE, at.saturating_add(1), format!("記録の操作「{name}」が {BENCH_MD} の表に無い"));
        }
    }
    for (at, row) in doc.lines().enumerate() {
        let Some(name) = row.strip_prefix("| ").and_then(|rest| rest.split(" | ").next()) else { continue };
        let measured = row.split(" | ").nth(1).is_some_and(|cell| cell.trim().parse::<f64>().is_ok());
        if measured && !name.contains("worker 4 個") && !recorded.contains(&name) {
            report.add(BENCH_MD, at.saturating_add(1), format!("操作「{name}」が、判定に使う記録 ({BENCH_GATE}) に無い"));
        }
    }
    let lib = repo.text(BENCH_LIB);
    let kept = [
        ("                Bench::decide(&baseline, &least, reference, LIMIT, true)", "記録との比較は、記録の幅で、測っていない操作も失敗にする"),
        ("                Bench::decide(&theirs, &least, reference, AGAINST_LIMIT, false)", "一つ前のコミットとの比較は、狭い幅で行う"),
        ("fn the_gate_fails_an_operation_past_the_limit_and_one_that_was_not_measured()", "判定の規則の試験"),
    ];
    for (needed, what) in kept {
        if !lib.contains(needed) {
            report.add(BENCH_LIB, 1, format!("遅くなっていないことの判定が崩れている: {what} (`{needed}`)"));
        }
    }
    if !bench.contains("    if let Err(why) = bench.finish() {\n        eprintln!(\"kome-bench: {why}\");\n        std::process::exit(1);") {
        report.add_at(repo, BENCH_RS, "fn main()", "測定の後に判定していない、または判定の失敗で終了コードを変えていない");
    }
    // CI のスクリプトは、記録との比較と、一つ前のコミットとの比較の両方を行う。一つ前のコミットを
    // 取れないときに、黙って通さない。
    let script = repo.text(BENCH_SH);
    let steps = [
        ("cargo bench -p kome-bench --locked -- --record \"$work/all.tsv\")", "一つ前のコミットを、同じ機械で測って最小値を足し込む"),
        ("    !/^#/ && ($1 in recorded) && $2 > recorded[$1] * 1.15 { print $1 }\n", "比較から外すのは、記録の比を幅より大きく引き上げた操作だけ"),
    ];
    for (needed, what) in steps {
        if !script.contains(needed) {
            report.add(BENCH_SH, 1, format!("性能の判定のスクリプトが足りない: {what} (`{}`)", needed.trim()));
        }
    }
}

fn verus_kept(repo: &Repo, report: &mut Report) {
    if !repo.has(VERUS_TSV) {
        report.add(VERUS_TSV, 1, "Verus が扱う関数の一覧が無い");
    }
    let listed: Vec<(&str, &str)> = repo
        .text(VERUS_TSV)
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .filter_map(|line| {
            let mut columns = line.split('\t');
            Some((columns.next()?, columns.next()?))
        })
        .collect();
    let (mut specs, mut trusted, mut loops, mut broken) = (0usize, 0usize, 0usize, 0usize);
    for (path, text) in repo.under(KOME_SRC, ".rs") {
        specs = specs.saturating_add(text.matches("verus_spec(").count());
        trusted = trusted.saturating_add(text.matches("verus_verify(external_body)").count());
        loops = loops.saturating_add(text.matches("verus_spec(\n        invariant\n").count());
        broken = broken.saturating_add(text.matches("fn verus_broken_").count());
        // 証明を飛ばす語は、どの形でも置かない。Mapping を中身の見えない型として渡す一か所だけを除く。
        let skip = ["ad", "mit()"].concat();
        for (at, line) in text.lines().enumerate() {
            let opaque = path == LOOKUP_RS && (line == "#[verifier::external_type_specification]" || line == "#[verifier::external_body]");
            let assumed = line.contains("assume(") && !line.contains("kani::assume(");
            let skips = assumed || line.contains(&skip) || line.contains("verifier::external") || line.contains("verus_verify(external)");
            if skips && !opaque {
                report.add(path, at.saturating_add(1), "Verus の証明を飛ばす指定を置かない");
            }
        }
    }
    let want = |kind: &str| listed.iter().filter(|(_, listed)| *listed == kind).count();
    if trusted != want("trusted") {
        report.add(VERUS_TSV, 1, format!("本体を確かめずに信じる定義が {trusted} 個 (一覧では {} 個)。増やさない", want("trusted")));
    }
    let functions = specs.saturating_sub(loops).saturating_sub(broken);
    if functions != listed.len() || want("verified") == 0 {
        report.add(VERUS_TSV, 1, format!("Verus の仕様つきの関数が {functions} 個 (一覧では {} 個)。減らさない。増やしたら一覧を直す", listed.len()));
    }
    if broken != 1 {
        report.add(LOOKUP_RS, 1, "Verus が確かめていることを示す、わざと誤った定義 (`fn verus_broken_..`) が一つ要る");
    }
    // 一覧の関数は、その名前で、仕様つきで、あるべきファイルにある。
    for (function, kind) in &listed {
        let mut parts = function.split("::").skip(1);
        let module = parts.next().unwrap_or("");
        let name = function.rsplit("::").next().unwrap_or("");
        let text = repo.text(&format!("{KOME_SRC}{module}.rs"));
        let defined = text.match_indices(&format!("fn {name}(")).any(|(at, _)| {
            let before = text.get(..at).unwrap_or("");
            let attribute = before.rfind("#[cfg_attr(verus_keep_ghost, ").map_or("", |from| before.get(from..).unwrap_or(""));
            let own = !attribute.is_empty() && !attribute.contains("\n}\n");
            let body_trusted = attribute.contains("verus_verify(external_body)");
            own && attribute.contains("verus_spec(") && (body_trusted == (*kind == "trusted"))
        });
        if !defined || (*kind != "verified" && *kind != "trusted") {
            report.add(VERUS_TSV, 1, format!("{function} ({kind}) に当たる、Verus の仕様つきの関数が無い"));
        }
    }
    let mem = repo.text(MEM_RS);
    if let Some(at) = mem.lines().position(|line| line.contains("partition_point") || line.contains("binary_search")) {
        report.add(MEM_RS, at.saturating_add(1), "マッピングの表は lookup.rs の証明した探索で引く");
    }
    let script = repo.text(VERUS_SH);
    for (needed, why) in [
        ("\"${check[@]}\" >\"$out/clean.json\"\n", "証明を走らせる"),
        ("if ! grep -q '\"errors\": 0,' \"$out/clean.json\"; then\n", "証明できない関数が無いことを確かめる"),
        ("if \"${check[@]}\" --cfg verus_broken >\"$out/broken.json\" 2>\"$out/broken.log\"; then\n", "わざと誤った定義が拒まれることを確かめる"),
        ("if [ \"$refused\" != \"$broken\" ] ||", "拒まれた定義が、わざと誤った定義とちょうど同じであることを確かめる"),
    ] {
        if !script.contains(needed) {
            report.add(VERUS_SH, 1, format!("Verus のスクリプトが足りない: {why}"));
        }
    }
}

fn refinements_kept(repo: &Repo, report: &mut Report) {
    if !repo.has(FLUX_TSV) {
        report.add(FLUX_TSV, 1, "篩型の仕様の一覧が無い");
    }
    let listed: Vec<(&str, usize, usize)> = repo
        .text(FLUX_TSV)
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .filter_map(|line| {
            let mut columns = line.split('\t');
            Some((columns.next()?, columns.next()?.parse().ok()?, columns.next()?.parse().ok()?))
        })
        .collect();
    let manifest = repo.text(KOME_MANIFEST);
    let table = manifest.split_once("[package.metadata.flux]\n").map_or("", |(_, rest)| rest.split("\n[").next().unwrap_or(""));
    for (needed, why) in [
        ("enabled = true", "Flux が kome を確かめない"),
        ("check-overflow = \"strict\"", "あふれを確かめないと、Flux は整数の上限を知らず、容量の式を証明できない"),
    ] {
        if !table.lines().any(|line| line == needed) {
            report.add(KOME_MANIFEST, 1, format!("[package.metadata.flux] に `{needed}` が無い: {why}"));
        }
    }
    let include = table.lines().find(|line| line.starts_with("include = ")).unwrap_or("");
    for (path, text) in repo.under(KOME_SRC, ".rs") {
        let specs = text.matches("flux::spec(").count();
        let trusted = text.matches("flux::trusted").count();
        let (want_specs, want_trusted) =
            listed.iter().find(|(listed, _, _)| *listed == path).map_or((0, 0), |(_, specs, trusted)| (*specs, *trusted));
        if specs != want_specs {
            report.add(path, 1, format!("篩型の仕様が {specs} 個 ({FLUX_TSV} では {want_specs} 個)。減らさない。増やしたら一覧を直す"));
        }
        if trusted != want_trusted {
            report.add(path, 1, format!("確かめずに信じる定義が {trusted} 個 ({FLUX_TSV} では {want_trusted} 個)。増やさない"));
        }
        // 無視の指定は、仕様を書いたまま確かめない抜け道になる。
        let skip = ["flux::", "ig", "nore"].concat();
        if let Some(at) = text.lines().position(|line| line.contains(&skip)) {
            report.add(path, at.saturating_add(1), "Flux に無視させない。確かめられない定義は仕様を書かない");
        }
        if specs == 0 {
            continue;
        }
        let name = path.strip_prefix(KOME_SRC).unwrap_or(path);
        let module = name.strip_suffix(".rs").unwrap_or(name);
        if !include.contains(&format!("\"src/{name}\"")) && !include.contains(&format!("\"def:{module}::")) {
            report.add(path, 1, format!("篩型の仕様があるが、{KOME_MANIFEST} の include に無いので Flux が確かめない"));
        }
        // わざと誤った定義がこのファイルで拒まれることを、flux.sh が確かめる。
        if text.matches("#[cfg(flux_broken)]\n").count() != 1 || text.matches("fn flux_broken_").count() != 1 {
            report.add(path, 1, "Flux がこのファイルを確かめていることを示す、わざと誤った定義 (`fn flux_broken_..`) が一つ要る");
        }
    }
    for (path, _, _) in &listed {
        if !repo.has(path) {
            report.add(FLUX_TSV, 1, format!("{path} が無い"));
        }
    }
    let script = repo.text(FLUX_SH);
    for (needed, why) in [
        ("RUSTFLAGS=\"-L $sysroot/lib\" cargo flux -p kome\n", "仕様を確かめる"),
        ("if RUSTFLAGS=\"-L $sysroot/lib --cfg flux_broken\" cargo flux -p kome >\"$log\" 2>&1; then\n", "わざと誤った定義が拒まれることを確かめる"),
        ("if [ \"$(echo \"$refused\" | sed 's/.*:://' | sort -u)\" != \"$expected\" ]; then\n", "拒まれた定義が、わざと誤った定義とちょうど同じであることを確かめる"),
    ] {
        if !script.contains(needed) {
            report.add(FLUX_SH, 1, format!("Flux のスクリプトが足りない: {why}"));
        }
    }
    if script.matches("    exit 1\n").count() != 2 {
        report.add(FLUX_SH, 1, "わざと誤った定義が通ったとき、または拒まれた定義が合わないときは失敗させる");
    }
}

fn backend_boundary(repo: &Repo, report: &mut Report) {
    let backend = repo.text(BACKEND_RS);
    // 境界は、ホストが呼べる公開の操作と問い合わせだけで書く。状態の中身には触れない。
    let allowed = ["translate", "next_mapping", "map_generation", "map", "unmap", "protect", "executable_views"];
    for (at, line) in backend.lines().enumerate() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        for (place, _) in line.match_indices("kernel.") {
            let name: String = line.get(place.saturating_add(7)..).unwrap_or("").chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            let called = line.get(place.saturating_add(7).saturating_add(name.len())..).is_some_and(|rest| rest.starts_with('('));
            if !called || !allowed.contains(&name.as_str()) {
                report.add(BACKEND_RS, at.saturating_add(1), format!("境界が Kernel の `{name}` に触れている。使えるのは {allowed:?} の呼び出しだけ"));
            }
        }
    }
    let kept = [
        ("    walk(kernel, process, addr, out.len(), Access::Read, |_, _, _| Ok(()))?;\n", "読み出しは、範囲の全体が読めることを先に確かめる"),
        ("    if generation != before {\n        observer.changed(", "権限が変わったときだけ知らせる"),
        ("        let seen = kernel.executable_views(span.backing(), span.offset(), len, |shown_in, range| code.invalidate(shown_in, range));", "監視つきの書き込みは、書いた実体を実行できる権限で見せている全ての場所 (別名) のコードを捨てさせる"),
    ];
    // backend が途中で失敗しても、失敗を返す前にコードを捨てさせる。
    let after_write = backend.split_once("    let written = walk(").map_or("", |(_, rest)| rest);
    if !after_write.split_once("    written?;\n").is_some_and(|(before, _)| before.contains("kernel.executable_views(")) {
        report.add_at(repo, BACKEND_RS, "pub fn guest_write_watched", "監視つきの書き込みが、backend の失敗を返す前にコードを捨てさせていない");
    }
    for (needed, what) in kept {
        if !backend.contains(needed) {
            report.add(BACKEND_RS, 1, format!("backend との境界の約束が崩れている: {what}"));
        }
    }
    let sim = repo.text(SIM_RS);
    let compared = [
        ("guest_write_watched(&self.k, &mut flaky, &mut dropped, process, GuestAddr::new(start), &data)", "書き込みを監視つきの経路で行う"),
    ];
    let reached = "let all = [\"code:kept\", \"code:dropped\", \"code:dropped-alias\", \"code:dropped-after-failure\"];";
    if !repo.text(SIM_TESTS_RS).contains(reached) {
        report.add(SIM_TESTS_RS, 1, format!("決定的シミュレーションが、別名と途中の失敗でコードを捨てさせる書き込みに到達することを確かめていない (`{reached}`)"));
    }
    for (needed, what) in compared {
        if !sim.contains(needed) {
            report.add(SIM_RS, 1, format!("決定的シミュレーションが backend との境界を確かめていない: {what} (`{needed}`)"));
        }
    }
    if sim.matches(SHADOW_CHECKED).count() != 3 {
        report.add(SIM_RS, 1, "決定的シミュレーションが、アドレス空間を変える三つの操作の全てで通知の結果を確かめていない");
    }
}

fn process_quota(repo: &Repo, report: &mut Report) {
    let process_rs = "crates/kome/src/process.rs";
    // 状態全体の上限のうち、プロセスごとに数えるものは、割り当てと合わせる関数を通してだけ読む。
    for (path, text) in kernel_files(repo) {
        if path == process_rs || path.ends_with("/audit.rs") || path.ends_with("/tests.rs") {
            continue;
        }
        for (at, line) in text.lines().enumerate() {
            for (direct, helper) in [("self.limits.holdings()", "holdings_limit"), ("self.limits.mappings()", "mappings_limit")] {
                if line.contains(direct) {
                    report.add(path, at.saturating_add(1), format!("`{direct}` を直接読んでいる。プロセスへの割り当ても見る `{helper}(process)` を使う"));
                }
            }
        }
    }
    let process = repo.text(process_rs);
    for needed in ["self.limits.holdings().min(quota)", "self.limits.mappings().min(quota)"] {
        if !process.contains(needed) {
            report.add_at(repo, process_rs, "pub(crate) fn holdings_limit", format!("上限と割り当ての小さい方を取っていない (`{needed}`)"));
        }
    }
    let sites = [
        ("crates/kome/src/thread.rs", ">= u64::from(owner.quota.threads()) {", "create_thread が、スレッドの数の割り当てを見ていない"),
        (IO_RS, "if ios > owner.quota.ios() {", "prepare_io が、I/O 要求の数の割り当てを見ていない"),
        ("crates/kome/src/mem.rs", "total <= owner.quota.mapped_bytes()", "map が、実体つきのバイト数の割り当てを見ていない"),
    ];
    for (path, needed, message) in sites {
        if !repo.text(path).contains(needed) {
            report.add(path, 1, message);
        }
    }
}

fn executor_contract(repo: &Repo, report: &mut Report) {
    let sched = repo.text(SCHED_RS);
    // 返却は、受理の前に理由を確かめ、受理した後の行き先を終了要求つきで決める。どちらも Kani が示した関数を通る。
    // 止まる理由の変種は、どれも文書の表に行がある。
    let architecture = repo.text(ARCHITECTURE_MD);
    let variants = sched.split("pub enum Stop {").nth(1).and_then(|rest| rest.split("\n}").next()).unwrap_or("");
    let mut count = 0usize;
    for line in variants.lines().map(str::trim).filter(|line| !line.starts_with("//") && !line.is_empty()) {
        let name: String = line.chars().take_while(|c| c.is_alphanumeric()).collect();
        count = count.saturating_add(1);
        if architecture.matches(&format!("| `Stop::{name}` |")).count() != 1 {
            report.add_at(repo, SCHED_RS, &format!("    {name}"), format!("止まる理由 {name} の行が、{ARCHITECTURE_MD} の表にちょうど一つ無い"));
        }
    }
    if count == 0 {
        report.add_at(repo, SCHED_RS, "pub enum Stop", "止まる理由の列挙を読めない");
    }
    // 貸す相手を選ぶ入口は二つで、どちらも同じ貸し出しを通る。
    if !sched.contains("        self.lend(worker, at).map(Some)") || sched.matches("self.lend(worker, at)").count() != 2 {
        report.add_at(repo, SCHED_RS, "pub fn grant(", "grant と grant_thread が、同じ貸し出し (lend) を通っていない");
    }
    // 取り出した文脈は複製できず、捨てると警告が出る。失敗しても実行権は返す。
    let exec = repo.text("crates/kome/src/exec.rs");
    if !exec.contains("#[must_use = \"取り出した文脈は Contexts::save で戻さなければならない\"]\n#[derive(Debug)]\npub struct Loaded<C> {") {
        report.add_at(repo, "crates/kome/src/exec.rs", "pub struct Loaded", "取り出した文脈 (Loaded) が複製できる、または捨てても警告が出ない");
    }
    if exec.matches("Err(hand_back(kernel, permit, ").count() != 3 {
        report.add_at(repo, EXEC_RS, "pub fn run_permit", "run_permit が、失敗を返す前に実行権を返していない経路がある");
    }
    // 実行を始める前と終える前に、実行権がこの Kernel の貸しているものかを確かめる。拒否した実行権と
    // 取り出していた文脈は、失わずに呼び出した側へ返す。
    let whole = [
        ("    if let Err(error) = kernel.lent(&permit) {\n        return Err(NotRun::Rejected { permit, loaded: None, error });\n    }\n    match contexts.load(&permit) {", "start_run が、文脈を取り出す前に実行権の所属を確かめていない"),
        ("    if let Err(error) = kernel.lent(&permit) {\n        return Err(NotRun::Rejected { permit, loaded: Some(loaded), error });\n    }\n    let thread = permit.thread();\n    if let Err(error) = contexts.save(loaded) {", "finish_run が、文脈を戻す前に実行権の所属を確かめていない、または拒否した文脈を返していない"),
    ];
    for (needed, what) in whole {
        if !exec.contains(needed) {
            report.add_at(repo, EXEC_RS, "pub fn start_run", what);
        }
    }
}

fn dup_free(repo: &Repo, report: &mut Report) {
    let known = match dup::known(repo.text(dup::KNOWN)) {
        Ok(known) => known,
        Err((line, why)) => {
            report.add(dup::KNOWN, line, format!("写しの一覧の行が決まりに合わない: {why}"));
            return;
        }
    };
    let found = dup::find(repo.all());
    for group in &found {
        if known.iter().any(|entry| dup::listed(group, entry)) {
            continue;
        }
        let Some(first) = group.members.first() else { continue };
        let others: Vec<String> = group.members.iter().skip(1).map(|member| format!("{}:{} {}", member.file, member.first, member.name)).collect();
        report.add(
            &first.file,
            first.first,
            format!(
                "同じコードが {} か所にある ({}、大きさ {}): `{}` と {}。一か所にまとめる。意図して持つなら {} に `{}` と書く",
                group.members.len(),
                group.kind.id(),
                group.size,
                first.name,
                others.join("、"),
                dup::KNOWN,
                dup::entry_line(group),
            ),
        );
    }
    for entry in &known {
        if !found.iter().any(|group| dup::listed(group, entry)) {
            report.add(dup::KNOWN, entry.line, "一覧にあるが、もう写しではない。行を消す");
        }
    }
}

fn channel_model(repo: &Repo, report: &mut Report) {
    // モデルは kome の名前を一つも使わない。端を持つ者と運ぶ資源は、型引数で受け取る。
    if let Some(found) = scanned(report, PIPES_RS, repo.text(PIPES_RS), true) {
        for (name, line) in &found.idents {
            if name == "kome" {
                report.add(PIPES_RS, *line, "通信路のモデルが kome を使っている。モデルは標準の入れ物と自分の判定だけで書く");
            }
        }
    }
    // 通知の資源の数は中身から決まる。外からの signal、タイマ、I/O の通知では変えられない。
    let refusals = repo.text(OBJECT_RS).matches("return Err(Error::Refused(Refusal::DrivenObject));").count();
    if refusals < 2 {
        report.add_at(repo, OBJECT_RS, "pub fn signal(", "通信路の通知の資源を、signal か通知先の指定で変えられる (DrivenObject で拒否する)");
    }
    // `driven` を決めるのは Object を作る所だけ。後から書き換えない。
    for (path, text) in kernel_files(repo) {
        for (at, line) in text.lines().enumerate() {
            if line.contains(".driven = ") && !path.ends_with("/tests.rs") {
                report.add(path, at.saturating_add(1), "通知の資源かどうか (driven) を後から書き換えている");
            }
        }
    }
}

const QUIET_OPS: [&str; 26] = [
    "grant",
    "set_quota",
    "grant_thread",
    "preempt_thread",
    "set_priority",
    "relinquish",
    "continue_thread",
    "suspend_thread",
    "unsuspend_thread",
    "signal",
    "interrupt_wait",
    "advance",
    "issue_io",
    "cancel_io",
    "complete",
    "publish_io",
    "release_object",
    "release_backing",
    "release_io",
    "release_channel",
    "send",
    "disarm_timer",
    "drop_timer",
    "terminate_thread",
    "exit_process",
    "poll_effect",
];

/// `text` の中の、バッククォートで囲んだ語と、その直前の文字列。
fn quoted(text: &str) -> Vec<(&str, &str)> {
    let mut found = Vec::new();
    let mut parts = text.split('`');
    let mut before = parts.next().unwrap_or("");
    while let Some(word) = parts.next() {
        found.push((before, word));
        before = parts.next().unwrap_or("");
    }
    found
}

fn docs_current(repo: &Repo, report: &mut Report) {
    let ledger = table(report, repo, LEDGER);
    let entries = ledger.get("invariant").and_then(toml::Value::as_array).map_or(&[][..], Vec::as_slice);
    let ids: BTreeSet<&str> = entries.iter().filter_map(|entry| entry.get("id")?.as_str()).collect();
    let architecture = repo.text(ARCHITECTURE_MD);
    for id in &ids {
        if !architecture.contains(id) {
            report.add(ARCHITECTURE_MD, 1, format!("台帳の {id} が責務の表に無い"));
        }
    }
    for path in [ARCHITECTURE_MD, MEMORY_MD, "README.md"] {
        if !repo.has(path) {
            report.add(path, 1, "文書が無い");
        }
        for (at, line) in repo.text(path).lines().enumerate() {
            let number = at.saturating_add(1);
            let lists_rules = line.contains("lint `");
            for (_, word) in quoted(line) {
                // 規則の名前の形 (小文字とハイフン)。クレートの名前 (kome-...) は規則ではない。
                let rule_like = word.contains('-')
                    && word.chars().all(|c| c.is_ascii_lowercase() || c == '-')
                    && !word.starts_with("kome-");
                if lists_rules && rule_like && !RULES.iter().any(|rule| rule.id == word) {
                    report.add(path, number, format!("規則 {word} は無い"));
                }
                let is_source = word.ends_with(".rs") && !word.contains('/');
                if is_source && !repo.has(&format!("{KOME_SRC}{word}")) {
                    report.add(path, number, format!("{KOME_SRC}{word} が無い"));
                }
            }
            // 「英大文字-数字」の形は台帳の識別子として読む。
            for token in line.split(|c: char| !c.is_ascii_alphanumeric() && c != '-') {
                let looks_like_id = token.split_once('-').is_some_and(|(head, tail)| {
                    !head.is_empty()
                        && head.chars().all(|c| c.is_ascii_uppercase())
                        && !tail.is_empty()
                        && tail.chars().all(|c| c.is_ascii_digit())
                });
                if looks_like_id && !ids.contains(token) {
                    report.add(path, number, format!("{token} は台帳に無い"));
                }
            }
        }
    }
}

fn quiet_operations(repo: &Repo, report: &mut Report) {
    let sim = repo.text(SIM_RS);
    let doc = repo.text(MEMORY_MD);
    let Some((may_allocate, never_allocate)) = doc.split_once("次の操作は確保しない。") else {
        report.add(MEMORY_MD, 1, "「次の操作は確保しない。」の表が無い");
        return;
    };
    let mut operations = BTreeSet::new();
    for (path, text) in kernel_files(repo) {
        let Some(found) = scanned(report, path, text, true) else { continue };
        operations.extend(found.kernel_ops.into_iter().map(|(name, _)| name));
    }
    for name in QUIET_OPS {
        if !operations.contains(name) {
            report.add(MEMORY_MD, 1, format!("確保しない操作の一覧にある {name} が Kernel に無い"));
        }
    }
    for name in &operations {
        // `apply` は他の操作への入口で、確保するかどうかは呼び出す操作で決まる。
        if name == "apply" {
            continue;
        }
        let quoted = format!("`{name}`");
        let quiet = QUIET_OPS.contains(&name.as_str());
        let (home, other, kind) =
            if quiet { (never_allocate, may_allocate, "確保しない") } else { (may_allocate, never_allocate, "確保しうる") };
        if !home.contains(&quoted) || other.contains(&quoted) {
            report.add(MEMORY_MD, 1, format!("操作 {name} は「{kind}操作」の表にだけ載せる"));
        }
        let marked = sim.contains(&format!("self.quiet(|k| k.{name}("));
        let bare = sim.contains(&format!("self.k.{name}("));
        if quiet && (!marked || bare) {
            report.add(SIM_RS, 1, format!("確保しない操作 {name} は self.quiet で挟んで呼ぶ (確保を数える印が付く)"));
        }
        if !quiet && marked {
            report.add(SIM_RS, 1, format!("確保しうる操作 {name} を self.quiet で挟まない"));
        }
    }
    let check = repo.text("checks/noalloc/tests/noalloc.rs");
    let counted = check.matches("assert_eq!(count(), before, ").count();
    if counted < 2 || !check.contains("static ALLOCATOR: Counting = Counting;") {
        report.add("checks/noalloc/tests/noalloc.rs", 1, "確保を数える検査が、印の間の確保が 0 であることを確かめていない");
    }
    if repo.text("checks/noalloc/src/lib.rs").matches("        note();\n").count() != 2 {
        report.add("checks/noalloc/src/lib.rs", 1, "確保と再確保の両方を数える");
    }
}

/// CI が必ず実行するコマンドと、それを置くジョブ。ステップの `run` は、この文字列ちょうどである。
const CI_RUNS: [(&str, &str); 17] = [
    ("lint", "cargo run -q --locked -p kome-lint"),
    ("lint", "cargo clippy --workspace --all-targets --locked -- -D warnings"),
    ("test", "scripts/ci/test.sh"),
    ("test", "git diff --exit-code -- tla"),
    ("test", "cargo test --locked --manifest-path checks/noalloc/Cargo.toml"),
    ("test", "cargo bench -p kome-bench --locked -- --smoke"),
    ("test", "scripts/ci/bench-gate.sh"),
    ("no-std", "cargo build -p kome --locked --target wasm32v1-none"),
    ("no-std", "cargo build -p kome --locked --target thumbv7em-none-eabi"),
    ("miri", "cargo miri test -p kome --locked --lib"),
    ("kani", "scripts/ci/kani.sh"),
    ("tla", "scripts/ci/tla.sh"),
    ("flux", "scripts/ci/flux-install.sh"),
    ("flux", "scripts/ci/flux.sh"),
    ("loom", "scripts/ci/loom.sh"),
    ("verus", "scripts/ci/verus-install.sh"),
    ("verus", "scripts/ci/verus.sh"),
];
/// ワークフロー、ジョブ、ステップに書いてよいキー。実行の条件や殻を変えるキー (`if`、
/// `continue-on-error`、`shell`、`defaults`、`working-directory`、ステップの `env`) は無い。
const CI_TOP_KEYS: [&str; 6] = ["name", "on", "permissions", "concurrency", "env", "jobs"];
const CI_JOB_KEYS: [&str; 5] = ["name", "runs-on", "timeout-minutes", "env", "steps"];
const CI_STEP_KEYS: [&str; 4] = ["name", "run", "uses", "with"];
/// ワークフローとジョブの `env` に置いてよい変数 (コマンドの意味を変えないもの)。
const CI_ENV: [&str; 4] = ["CARGO_TERM_COLOR", "CARGO_INCREMENTAL", "RUSTUP_TOOLCHAIN", "MIRIFLAGS"];
/// 試験のスクリプトが必ず行うこと: 全クレートの試験と、走った試験の照合。
const TEST_SH: &str = "scripts/ci/test.sh";
const TEST_STEPS: [&str; 2] = ["cargo test --workspace --locked 2>&1 | tee \"$log\"", "cargo run -q --locked -p kome-lint -- ran \"$log\""];
const CI_BANNED: [(&str, &str); 7] = [
    ("| grep -q", "grep -q は一致したところで読むのをやめ、書く側が SIGPIPE で失敗する (pipefail のもとで、出力が長いときだけ落ちる)。`grep -q … <<<\"$out\"` と書く"),
    ("continue-on-error", "失敗を見逃す設定を置かない"),
    ("|| true", "失敗を握りつぶさない"),
    ("cancel-in-progress: true", "main への push が続くと実行が終わらなくなる。取り消さない"),
    ("npm ", "npm は使わない"),
    ("npx ", "npm は使わない"),
    ("cargo fmt", "cargo fmt は使わない"),
];

fn yaml_keys(node: &yaml_rust2::Yaml) -> Vec<&str> {
    node.as_hash().map(|hash| hash.keys().filter_map(yaml_rust2::Yaml::as_str).collect()).unwrap_or_default()
}

/// CI の構造: 必須のコマンドが、決まったジョブの、条件の付かないステップの `run` そのものであること。
fn ci_structure(repo: &Repo, report: &mut Report) {
    let docs = match yaml_rust2::YamlLoader::load_from_str(repo.text(CI)) {
        Ok(docs) => docs,
        Err(error) => {
            report.add(CI, 1, format!("YAML として読めない: {error}"));
            return;
        }
    };
    let Some(doc) = docs.first() else {
        report.add(CI, 1, "CI の定義が無い");
        return;
    };
    for key in yaml_keys(doc) {
        if !CI_TOP_KEYS.contains(&key) {
            report.add_at(repo, CI, key, format!("ワークフローに `{key}` を置かない (実行のしかたを変えるキー)"));
        }
    }
    let on_main = doc["on"]["push"]["branches"].as_vec().is_some_and(|branches| branches.iter().any(|b| b.as_str() == Some("main")));
    if !on_main || !yaml_keys(&doc["on"]).contains(&"pull_request") {
        report.add_at(repo, CI, "on:", "CI は main への push と pull request で動く");
    }
    if doc["concurrency"]["cancel-in-progress"].as_bool() != Some(false) {
        report.add_at(repo, CI, "concurrency:", "`cancel-in-progress: false` にする (main への push が続くと実行が終わらなくなる)");
    }
    let env_ok = |node: &yaml_rust2::Yaml| yaml_keys(node).into_iter().all(|key| CI_ENV.contains(&key));
    if !env_ok(&doc["env"]) {
        report.add_at(repo, CI, "env:", "ワークフローの env に、決めた変数以外を置かない");
    }
    let jobs = &doc["jobs"];
    for (id, job) in jobs.as_hash().into_iter().flatten() {
        let id = id.as_str().unwrap_or("");
        let at = format!("  {id}:");
        for key in yaml_keys(job) {
            if !CI_JOB_KEYS.contains(&key) {
                report.add_at(repo, CI, &at, format!("ジョブ {id} に `{key}` を置かない (実行の条件や失敗の扱いを変えるキー)"));
            }
        }
        if job["runs-on"].as_str() != Some("ubuntu-latest") || job["timeout-minutes"].as_i64().is_none() {
            report.add_at(repo, CI, &at, format!("ジョブ {id} は GitHub の ubuntu-latest で動かし、制限時間を持つ"));
        }
        if !env_ok(&job["env"]) {
            report.add_at(repo, CI, &at, format!("ジョブ {id} の env に、決めた変数以外を置かない"));
        }
        for step in job["steps"].as_vec().into_iter().flatten() {
            for key in yaml_keys(step) {
                if !CI_STEP_KEYS.contains(&key) {
                    report.add_at(repo, CI, &at, format!("ジョブ {id} のステップに `{key}` を置かない (実行の条件や殻を変えるキー)"));
                }
            }
        }
    }
    for (id, command) in CI_RUNS {
        let steps = jobs[id]["steps"].as_vec().map_or(&[][..], Vec::as_slice);
        if !steps.iter().any(|step| step["run"].as_str().map(str::trim) == Some(command)) {
            report.add(CI, 1, format!("CI のジョブ {id} に、`run` がちょうど `{command}` のステップが無い"));
        }
    }
}

fn ci_required(repo: &Repo, report: &mut Report) {
    ci_structure(repo, report);
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
                // リポジトリの中の Action (`./…`) は、同じコミットの一部である。
                if !pinned && !action.starts_with("./") {
                    report.add(path, number, "Action はコミットのハッシュで固定する");
                }
            }
        }
    }
}
