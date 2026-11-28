//! Lean のモデル (lean/Kome/Tlb.lean) と、実際の `Kernel` と `SoftTlb` の突き合わせ。
//!
//! Lean では「操作をどんな順に何回行っても、表に当たるアクセスは今のアドレス空間で許されている」を
//! モデルについて証明している。その証明が実装について何かを言うのは、モデルが実装と同じ動きをする
//! ときだけである。ここでは、種から作った操作の列を両方に行い、操作のたびに次の `Line` が一致する
//! ことを確かめる。
//!
//! - 操作の結果の種類 (受理した、重なりで拒んだ、埋めた、基準の経路と同じ失敗、遅い経路、ほか)。
//! - 全てのプロセスの全ての頁が、読み出しと書き込みで表に当たるか。当たるなら、実体のどの頁か。
//!
//! モデルを実行するのは `mirror` (lean/Mirror.lean)。scripts/ci/lean.sh がビルドし、`KOME_LEAN_MIRROR`
//! でその場所を渡す。渡されなければ何も確かめない (lean.sh は、照合した行が出たことまで見る)。
#![cfg(test)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::Command;

use kome::{
    Access, AddrWidth, BackingId, BackingOffset, BackingPage, CodePages, Error, ExitCode, GuestAddr, GuestRange, InstanceId, Kernel, Limits,
    LinearLayout, MapChange, MapGen, MapSource, NotFast, PageSize, ProcessId, Prot, Refusal, SoftTlb, SpaceObserver, map_observed,
    protect_observed, unmap_observed,
};
use kome_testkit::noise;

const PAGE: u64 = 0x1000;
/// 表の項目の数。頁の数より少なくして、同じ項目を取り合う頁が出るようにする。
const SLOTS: usize = 4;
const PROCS: usize = 3;
/// 比べるゲストの頁の数。
const PAGES: u64 = 10;
/// 実体の頁の数。
const SPOTS: u64 = 12;

/// 全ての表へ、同じ変更を知らせる。
struct Tables<'a>(&'a mut [SoftTlb<u32>]);

impl SpaceObserver for Tables<'_> {
    fn changed(&mut self, process: ProcessId, generation: MapGen, change: MapChange) {
        for tlb in self.0.iter_mut() {
            tlb.changed(process, generation, change);
        }
    }

    fn cleared(&mut self, process: ProcessId) {
        for tlb in self.0.iter_mut() {
            tlb.cleared(process);
        }
    }
}

/// 実装の側の世界。プロセスは番号で呼び、実体は一つ (その頁の番号が、モデルの `spot`)。
struct World {
    kernel: Kernel,
    keeper: ProcessId,
    backing: BackingId,
    layout: LinearLayout,
    code: CodePages,
    translated: BTreeMap<u64, BackingPage>,
    processes: Vec<ProcessId>,
    tlbs: Vec<SoftTlb<u32>>,
}

fn page_size() -> PageSize {
    PageSize::new(PAGE).unwrap()
}

fn pages(first: u64, count: u64) -> GuestRange {
    GuestRange::new(GuestAddr::new(first * PAGE), count * PAGE).unwrap()
}

impl World {
    fn new() -> World {
        let mut kernel = Kernel::new(InstanceId::new(1), Limits::DEFAULT).unwrap();
        // 実体は、終了しないプロセスが作って持ち続ける。
        let (keeper, _) = kernel.create_process(AddrWidth::Bits32, page_size()).unwrap();
        let backing = kernel.create_backing(keeper, SPOTS * PAGE).unwrap();
        let mut layout = LinearLayout::new(SPOTS * PAGE, page_size());
        layout.place(backing, SPOTS * PAGE).unwrap();
        let mut world =
            World { kernel, keeper, backing, layout, code: CodePages::new(), translated: BTreeMap::new(), processes: Vec::new(), tlbs: Vec::new() };
        for _ in 0..PROCS {
            let (process, tlb) = world.fresh();
            world.processes.push(process);
            world.tlbs.push(tlb);
        }
        world
    }

    /// 新しいプロセスと、その表。
    fn fresh(&mut self) -> (ProcessId, SoftTlb<u32>) {
        let (process, _) = self.kernel.create_process(AddrWidth::Bits32, page_size()).unwrap();
        self.kernel.retain_backing(process, self.backing).unwrap();
        (process, SoftTlb::new(process, page_size(), SLOTS).unwrap())
    }

    /// 全てのプロセスの全ての頁の、読み出しと書き込みの当たり方。
    fn hits(&self) -> String {
        let cell = |tlb: &SoftTlb<u32>, page: u64, access| {
            tlb.probe(GuestAddr::new(page * PAGE + 5), access).map_or("-".to_owned(), |position| (position.get() / PAGE).to_string())
        };
        let per_process = self.tlbs.iter().map(|tlb| {
            let cells: Vec<String> = (0..PAGES).map(|page| format!("{}:{}", cell(tlb, page, Access::Read), cell(tlb, page, Access::Write))).collect();
            cells.join(" ")
        });
        per_process.collect::<Vec<String>>().join(" | ")
    }

    /// 一つの操作を行い、結果の種類を返す。
    fn apply(&mut self, op: &Op) -> String {
        match *op {
            Op::Map { p, first, count, spot, prot: (r, w, x) } => {
                let source = MapSource::Backing { backing: self.backing, offset: BackingOffset::new(spot * PAGE) };
                match map_observed(&mut self.kernel, &mut Tables(&mut self.tlbs), self.processes[p], pages(first, count), source, Prot::new(r, w, x)) {
                    Ok(_) => "mapped".to_owned(),
                    Err(Error::Refused(Refusal::Overlap)) => "overlap".to_owned(),
                    Err(other) => panic!("map が {other:?}"),
                }
            }
            Op::Unmap { p, first, count } => {
                unmap_observed(&mut self.kernel, &mut Tables(&mut self.tlbs), self.processes[p], pages(first, count)).unwrap();
                "done".to_owned()
            }
            Op::Protect { p, first, count, prot: (r, w, x) } => {
                match protect_observed(&mut self.kernel, &mut Tables(&mut self.tlbs), self.processes[p], pages(first, count), Prot::new(r, w, x)) {
                    Ok(_) => "protected".to_owned(),
                    Err(Error::Refused(Refusal::NotMapped | Refusal::NotBacked)) => "uncovered".to_owned(),
                    Err(other) => panic!("protect が {other:?}"),
                }
            }
            Op::Fill { p, page, write } => {
                let access = if write { Access::Write } else { Access::Read };
                match self.tlbs[p].fill(&self.kernel, &self.layout, &self.code, GuestAddr::new(page * PAGE + 9), access) {
                    Ok(_) => "filled".to_owned(),
                    Err(NotFast::Fault(_)) => "fault".to_owned(),
                    Err(NotFast::Slow) => "slow".to_owned(),
                    Err(NotFast::Kernel(error)) => panic!("fill が {error:?}"),
                }
            }
            Op::Translate { p, page } => match self.tlbs[p].protect_code(&self.kernel, &mut self.code, GuestAddr::new(page * PAGE + 1)) {
                Ok(code_page) => {
                    for tlb in &mut self.tlbs {
                        tlb.bar_writes(code_page);
                    }
                    self.translated.insert(code_page.offset().get() / PAGE, code_page);
                    "translated".to_owned()
                }
                Err(NotFast::Fault(_)) => "fault".to_owned(),
                Err(other) => panic!("protect_code が {other:?}"),
            },
            Op::Release { spot } => {
                if let Some(code_page) = self.translated.remove(&spot) {
                    assert!(self.code.release(code_page));
                }
                "done".to_owned()
            }
            Op::Take { p } => {
                let mut spots = BTreeSet::new();
                self.tlbs[p].take_dirty(|page| {
                    spots.insert(page.offset().get() / PAGE);
                });
                let listed: Vec<String> = spots.iter().map(u64::to_string).collect();
                format!("dirty {}", listed.join(" "))
            }
            Op::Flush { p } => {
                self.tlbs[p].flush();
                "done".to_owned()
            }
            Op::Cleared { p } => {
                // プロセスを終了させ、同じ番号に新しいプロセスを置く。
                let old = self.processes[p];
                self.kernel.exit_process(old, ExitCode::new(0)).unwrap();
                while self.kernel.poll_effect().is_some() {}
                Tables(&mut self.tlbs).cleared(old);
                let (process, tlb) = self.fresh();
                self.processes[p] = process;
                self.tlbs[p] = tlb;
                "done".to_owned()
            }
        }
    }
}

type Flags = (bool, bool, bool);

/// 一つの操作。`mirror` へ渡す行は [`Op::line`]。
#[derive(Clone, Copy, Debug)]
enum Op {
    Map { p: usize, first: u64, count: u64, spot: u64, prot: Flags },
    Unmap { p: usize, first: u64, count: u64 },
    Protect { p: usize, first: u64, count: u64, prot: Flags },
    Fill { p: usize, page: u64, write: bool },
    Translate { p: usize, page: u64 },
    Release { spot: u64 },
    Take { p: usize },
    Flush { p: usize },
    Cleared { p: usize },
}

impl Op {
    fn line(&self) -> String {
        let bit = u8::from;
        match *self {
            Op::Map { p, first, count, spot, prot: (r, w, x) } => format!("map {p} {first} {count} {spot} {} {} {}", bit(r), bit(w), bit(x)),
            Op::Unmap { p, first, count } => format!("unmap {p} {first} {count}"),
            Op::Protect { p, first, count, prot: (r, w, x) } => format!("protect {p} {first} {count} {} {} {}", bit(r), bit(w), bit(x)),
            Op::Fill { p, page, write } => format!("fill {p} {page} {}", bit(write)),
            Op::Translate { p, page } => format!("translate {p} {page}"),
            Op::Release { spot } => format!("release {spot}"),
            Op::Take { p } => format!("take {p}"),
            Op::Flush { p } => format!("flush {p}"),
            Op::Cleared { p } => format!("cleared {p}"),
        }
    }
}

/// バイト列から、操作の列を作る。権限は、読める・書ける・実行できるものに寄せる。
fn ops(bytes: &[u8]) -> Vec<Op> {
    let mut feed = bytes.iter().copied();
    let mut next = move |bound: u64| u64::from(feed.next().unwrap_or(0)) % bound;
    let mut out = Vec::new();
    // 一つの操作は、多くて 6 バイト使う。
    for _ in 0..bytes.len() / 6 {
        let kind = next(20);
        let p = usize::try_from(next(u64::try_from(PROCS).unwrap())).unwrap();
        let prot = |next: &mut dyn FnMut(u64) -> u64| {
            let bits = next(16);
            // 16 通りのうち 8 通りは全ての権限。残りは 3 ビットの全ての組。
            if bits >= 8 { (true, true, true) } else { (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0) }
        };
        out.push(match kind {
            0..=3 => {
                let count = next(3) + 1;
                Op::Map { p, first: next(PAGES - count + 1), count, spot: next(SPOTS - count + 1), prot: prot(&mut next) }
            }
            4 => {
                let count = next(3) + 1;
                Op::Unmap { p, first: next(PAGES - count + 1), count }
            }
            5 | 6 => {
                let count = next(2) + 1;
                Op::Protect { p, first: next(PAGES - count + 1), count, prot: prot(&mut next) }
            }
            7..=13 => Op::Fill { p, page: next(PAGES), write: next(2) == 1 },
            14 | 15 => Op::Translate { p, page: next(PAGES) },
            16 => Op::Release { spot: next(SPOTS) },
            17 => Op::Take { p },
            18 => Op::Flush { p },
            _ if next(3) == 0 => Op::Cleared { p },
            _ => Op::Fill { p, page: next(PAGES), write: true },
        });
    }
    out
}

/// 終了しないプロセスは、比べる対象に入らない。使われていることだけ確かめる。
fn keeper_lives(world: &World) {
    assert!(world.kernel.backing_len(world.backing).is_ok());
    assert!(world.kernel.mapping_count(world.keeper).is_ok());
}

#[test]
fn the_lean_model_of_the_table_moves_like_the_kernel() {
    let Some(mirror) = std::env::var_os("KOME_LEAN_MIRROR") else {
        eprintln!("KOME_LEAN_MIRROR が無いので、Lean のモデルとの突き合わせは走らせない (scripts/ci/lean.sh が走らせる)");
        return;
    };
    // 実装の側: 列ごとに新しい世界で操作を行い、行を作る。
    let mut script = String::new();
    let mut expected = Vec::new();
    let mut seen = BTreeSet::new();
    let (mut runs, mut steps) = (0u32, 0usize);
    for seed in 0..300u64 {
        let mut world = World::new();
        writeln!(script, "world {SLOTS} {PROCS} {PAGES} {SPOTS}").unwrap();
        expected.push("world".to_owned());
        for op in ops(&noise(seed, 240)) {
            writeln!(script, "{}", op.line()).unwrap();
            let told = world.apply(&op);
            world.kernel.audit().unwrap();
            seen.insert(told.split(' ').next().unwrap().to_owned() + if told.starts_with("dirty ") && told.len() > 6 { "+" } else { "" });
            expected.push(format!("{told} ; {}", world.hits()));
            steps += 1;
        }
        keeper_lives(&world);
        runs += 1;
    }
    // 突き合わせが空回りしていないこと: 全ての種類の結果を通っている。
    let all = ["mapped", "overlap", "done", "protected", "uncovered", "filled", "fault", "slow", "translated", "dirty", "dirty+"];
    assert_eq!(seen, all.iter().map(|kind| (*kind).to_owned()).collect::<BTreeSet<String>>());
    let hits = expected.iter().filter(|line| line.split(" ; ").nth(1).is_some_and(|cells| cells.bytes().any(|b| b.is_ascii_digit()))).count();
    assert!(hits * 4 > steps, "表に当たる状態が少ない: {steps} 行のうち {hits} 行");

    // モデルの側。
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("lean-tlb-ops.txt");
    std::fs::write(&path, &script).unwrap();
    let output = Command::new(&mirror).arg(&path).output().unwrap_or_else(|error| panic!("{} を実行できない: {error}", mirror.to_string_lossy()));
    assert!(output.status.success(), "mirror が失敗した: {}", String::from_utf8_lossy(&output.stderr));
    let told = String::from_utf8(output.stdout).unwrap();
    let got: Vec<&str> = told.lines().collect();
    let sent: Vec<&str> = script.lines().collect();
    for (at, (model, real)) in got.iter().zip(&expected).enumerate() {
        if model != real {
            let from = sent[..=at].iter().rposition(|line| line.starts_with("world ")).unwrap();
            panic!("{} 行めの操作 `{}` の後が違う\nモデル: {model}\n実装:   {real}\nこの列の操作:\n{}", at + 1, sent[at], sent[from..=at].join("\n"));
        }
    }
    assert_eq!(got.len(), expected.len(), "行の数が違う");
    println!("lean: {runs} 本の列、{steps} 個の操作で、Lean のモデルと実装の動きを照合した");
}
