//! TLA+ のモデルと実装の突き合わせ。
//!
//! TLC が書き出したモデルの状態グラフ (全ての状態と遷移) を読み、実際の `Kernel` を同じ操作で
//! 動かして得た状態と遷移の集合が、それと一致することを確かめる。
//!
//! - 実装が行う遷移は、全てモデルにある (実装がモデルに無い動きをしない)。
//! - モデルの遷移は、全て実装が行う (モデルが実装に無い動きを仮定していない)。
//!
//! モデルの変数と実装の値の対応は、各モデルの `state` を作る所に書いてある。一覧は
//! docs/tla-conformance.md。確かめ方の限界もそこに書く: モデルの一つの状態につき、
//! 実装の状態は最初に辿り着いた一つだけを代表として調べる。
//!
//! 状態グラフは scripts/ci/tla.sh が作り、`KOME_TLA_DUMPS` でその場所を渡す。渡されなければ
//! 何も確かめない (tla.sh は、確かめた行が出たことまで見る)。

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Debug;
use std::{eprintln, format, println};

use crate::thread::Run;
use crate::*;

/// モデルの状態グラフ。状態は、変数ごとの行を名前順に並べた文章で表す。
struct Graph {
    initial: String,
    ids: BTreeMap<String, u32>,
    edges: BTreeSet<(u32, u32)>,
}

/// TLC の `-dump dot` の出力を読む。同じ状態への遷移 (何も変えない歩み) は捨てる。
fn read_graph(text: &str) -> Graph {
    let mut by_hash: BTreeMap<&str, u32> = BTreeMap::new();
    let mut ids = BTreeMap::new();
    let mut initial = None;
    let mut raw_edges = Vec::new();
    for line in text.lines() {
        let Some((head, rest)) = line.split_once(' ') else { continue };
        if !head.trim_start_matches('-').bytes().all(|b| b.is_ascii_digit()) || head.is_empty() {
            continue;
        }
        if let Some(rest) = rest.strip_prefix("-> ") {
            let (to, _) = rest.split_once(' ').unwrap();
            raw_edges.push((head, to));
        } else if let Some(rest) = rest.strip_prefix("[label=\"") {
            let (label, tail) = rest.rsplit_once('"').unwrap();
            let state = normalize(label);
            let id = u32::try_from(ids.len()).unwrap();
            assert!(ids.insert(state.clone(), id).is_none(), "同じ状態が二度ある: {state}");
            by_hash.insert(head, id);
            if tail.contains("style = filled") {
                assert!(initial.replace(state).is_none(), "初期状態が二つある");
            }
        }
    }
    let edges = raw_edges.iter().map(|(from, to)| (by_hash[from], by_hash[to])).filter(|(from, to)| from != to).collect();
    Graph { initial: initial.expect("初期状態が無い"), ids, edges }
}

/// dot のラベルを、変数ごとの行を名前順に並べた文章へ直す。
fn normalize(label: &str) -> String {
    let mut plain = String::new();
    let mut chars = label.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            plain.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => plain.push('\n'),
            Some(other) => plain.push(other),
            None => panic!("ラベルが \\ で終わっている: {label}"),
        }
    }
    // 長い値は、TLC が字下げして複数の行に折る。変数の区切りは行頭の `/\`。
    let mut lines: Vec<String> = plain.split("\n/\\ ").map(|line| squeeze(line.strip_prefix("/\\ ").unwrap_or(line))).collect();
    lines.sort_unstable();
    lines.join("\n")
}

/// 空白を全て取る。値の折り方と詰め方の違いを消す (文字列の値に空白は無い)。
fn squeeze(line: &str) -> String {
    line.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 変数ごとの行から、状態を表す文章を作る。
fn state_text(lines: Vec<String>) -> String {
    let mut lines: Vec<String> = lines.iter().map(|line| squeeze(line)).collect();
    lines.sort_unstable();
    lines.join("\n")
}

/// モデル値 t1, t2, ... を定義域とする関数の、TLC の書き方。
fn per_thread<T>(values: &[T], show: impl Fn(&T) -> String) -> String {
    let cells: Vec<String> = values.iter().zip(1..).map(|(value, n)| format!("t{n} :> {}", show(value))).collect();
    format!("({})", cells.join(" @@ "))
}

fn truth(value: bool) -> &'static str {
    if value { "TRUE" } else { "FALSE" }
}

fn quoted(name: &str) -> String {
    format!("\"{name}\"")
}

/// 実装を、モデルと同じ操作で動かすもの。
trait Driven {
    type Op: Copy + Debug;
    /// モデルの名前 (状態グラフのファイル名)。
    const NAME: &'static str;
    /// 試す操作の全て。
    fn ops() -> Vec<Self::Op>;
    /// 初期状態から `path` を順に行い、最後の状態を返す。最後の操作を実装が受け付けなければ `None`。
    /// 途中の操作は、前に受け付けた操作の列なので、必ず受け付ける。
    fn run(path: &[Self::Op]) -> Option<String>;
}

/// 実装を初期状態から幅優先で動かして、状態と遷移を集める。モデルに無い状態や遷移に
/// 出会ったら、そこまでの操作の列を添えて止まる。
fn explore<D: Driven>(model: &Graph) -> Result<BTreeSet<(u32, u32)>, String> {
    let start = D::run(&[]).ok_or("初期状態を作れない")?;
    if start != model.initial {
        return Err(format!("初期状態が違う\n実装:\n{start}\nモデル:\n{}", model.initial));
    }
    let ops = D::ops();
    let mut seen = BTreeSet::new();
    let mut edges = BTreeSet::new();
    let mut queue = VecDeque::new();
    let first = model.ids[&start];
    seen.insert(first);
    queue.push_back((first, Vec::new()));
    while let Some((from, path)) = queue.pop_front() {
        for op in &ops {
            let mut longer = path.clone();
            longer.push(*op);
            let Some(next) = D::run(&longer) else { continue };
            let Some(to) = model.ids.get(&next).copied() else {
                return Err(format!("実装が、モデルに無い状態へ進んだ\n操作: {longer:?}\n状態:\n{next}"));
            };
            if to == from {
                continue;
            }
            if !model.edges.contains(&(from, to)) {
                return Err(format!("実装が、モデルに無い遷移をした\n操作: {longer:?}\n行き先:\n{next}"));
            }
            edges.insert((from, to));
            if seen.insert(to) {
                queue.push_back((to, longer));
            }
        }
    }
    Ok(edges)
}

/// モデルの遷移で、実装が行わなかったもの。
fn never_taken(model: &Graph, taken: &BTreeSet<(u32, u32)>) -> Result<(), String> {
    let missing: Vec<&(u32, u32)> = model.edges.difference(taken).collect();
    let Some((from, to)) = missing.first() else { return Ok(()) };
    let name = |id: u32| model.ids.iter().find(|(_, at)| **at == id).map(|(state, _)| state.as_str()).unwrap_or("?");
    Err(format!(
        "モデルの遷移のうち {} 個を、実装が行わない。最初の一つ:\n{}\n  から\n{}",
        missing.len(),
        name(*from),
        name(*to)
    ))
}

/// 一つのモデルを突き合わせる。状態グラフの場所が渡されていなければ何もしない。
fn conform<D: Driven>() {
    let Some(dir) = std::env::var_os("KOME_TLA_DUMPS") else {
        eprintln!("KOME_TLA_DUMPS が無いので、{} の突き合わせは走らせない (scripts/ci/tla.sh が走らせる)", D::NAME);
        return;
    };
    let path = std::path::Path::new(&dir).join(format!("{}.dot", D::NAME));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    let model = read_graph(&text);
    let taken = explore::<D>(&model).unwrap_or_else(|why| panic!("{}: {why}", D::NAME));
    never_taken(&model, &taken).unwrap_or_else(|why| panic!("{}: {why}", D::NAME));
    assert_eq!(taken, model.edges);
    // この突き合わせが違いを見分けることの確認: モデルの遷移を一つ足しても、一つ抜いても、
    // 行き先を一つ変えても、一致しなくなる。
    let (from, to) = *model.edges.iter().next().expect("遷移が一つも無い");
    let mut fewer = read_graph(&text);
    fewer.edges.remove(&(from, to));
    assert!(explore::<D>(&fewer).is_err(), "モデルから遷移を一つ抜いても通った");
    let mut more = read_graph(&text);
    let absent = (0..u32::try_from(model.ids.len()).unwrap())
        .map(|other| (from, other))
        .find(|edge| edge.0 != edge.1 && !model.edges.contains(edge))
        .expect("最初の状態が全ての状態へ進む");
    more.edges.insert(absent);
    assert!(never_taken(&more, &taken).is_err(), "モデルに遷移を一つ足しても通った");
    println!("conform {}: 状態 {} 個、遷移 {} 個が一致した", D::NAME, model.ids.len(), model.edges.len());
}

const PAGE: u64 = 0x1000;
const CODE: ExitCode = ExitCode::new(7);
const DONE: IoOutcome = IoOutcome::Done { transferred: 12 };

fn kernel(workers: u32) -> Kernel {
    Kernel::new(InstanceId::new(1), Limits::DEFAULT.with_workers(workers)).unwrap()
}

fn process(k: &mut Kernel) -> ProcessId {
    k.create_process(AddrWidth::Bits32, PageSize::new(PAGE).unwrap()).unwrap().0
}

/// スレッドを `N` 個作る。
fn threads<const N: usize>(k: &mut Kernel, p: ProcessId) -> [ThreadId; N] {
    [(); N].map(|()| k.create_thread(p, Start::Ready).unwrap().0)
}

/// 実行待ちの先頭のスレッドを一度実行させ、サービス中にする。
fn trap(k: &mut Kernel) -> ThreadId {
    let permit = k.grant(WorkerId::new(0)).unwrap().unwrap();
    let thread = permit.thread();
    assert_eq!(k.relinquish(permit, Stop::Trap, Budget::new(1)).unwrap(), ThreadFate::Servicing);
    thread
}

fn drain(k: &mut Kernel) -> Vec<Effect> {
    core::iter::from_fn(|| k.poll_effect()).collect()
}

// ---- WaitRace ----

const OBJECTS: [&str; 2] = ["a", "b"];
const ORDERS: [&[usize]; 4] = [&[0], &[1], &[0, 1], &[1, 0]];
/// tla/WaitRace.cfg の MaxSignals。モデルが探索を有限にするための上限で、実装の上限ではない。
const MAX_SIGNALS: u64 = 2;

#[derive(Clone, Copy, Debug)]
enum WaitOp {
    /// スレッドが、`ORDERS` の一つを対象の列として待機を始める。
    Wait(usize, usize, WaitMode),
    /// 資源の数を一つ増やす。
    Signal(usize),
    /// スレッドの待機の期限が来る。
    Timeout(usize),
}

struct WaitRace;

impl Driven for WaitRace {
    type Op = WaitOp;
    const NAME: &'static str = "WaitRace";

    fn ops() -> Vec<WaitOp> {
        let mut ops = Vec::new();
        for thread in 0..3 {
            for order in 0..ORDERS.len() {
                ops.push(WaitOp::Wait(thread, order, WaitMode::Any));
                ops.push(WaitOp::Wait(thread, order, WaitMode::All));
            }
            ops.push(WaitOp::Timeout(thread));
        }
        ops.extend([WaitOp::Signal(0), WaitOp::Signal(1)]);
        ops
    }

    fn run(path: &[WaitOp]) -> Option<String> {
        let mut k = kernel(1);
        let p = process(&mut k);
        let made: [ThreadId; 3] = threads(&mut k, p);
        for thread in made {
            assert_eq!(trap(&mut k), thread);
        }
        let objects = [(); 2].map(|()| k.create_object(p, SignalSpec::new(SignalMode::Consume, 0, 2)).unwrap());
        // 期限は待機を始めるときに決まる。どの待機の期限も、どの順でも来られるように、この列の中で
        // k 番目に期限が来るスレッドの期限を時刻 k にしておく。時刻は期限が来るときにだけ進める。
        let mut deadline = [None; 3];
        for (op, nth) in path.iter().filter(|op| matches!(op, WaitOp::Timeout(_))).zip(1u64..) {
            if let WaitOp::Timeout(thread) = op {
                deadline[*thread].get_or_insert(nth);
            }
        }
        let mut sent = [0u64; 2];
        let mut want: [Option<(usize, WaitMode)>; 3] = [None; 3];
        let mut queue: Vec<usize> = Vec::new();
        let mut outcomes = [0u32; 3];
        let mut took = [[false; 2]; 3];
        let mut timeouts = 0u64;
        let mut settle = |at: usize, outcome: WaitOutcome, want: &[Option<(usize, WaitMode)>; 3], queue: &mut Vec<usize>| {
            let (order, _) = want[at].unwrap();
            outcomes[at] = outcomes[at].checked_add(1).unwrap();
            queue.retain(|waiting| *waiting != at);
            match outcome {
                WaitOutcome::Signaled { index } => took[at][ORDERS[order][usize::try_from(index).unwrap()]] = true,
                WaitOutcome::AllSignaled => ORDERS[order].iter().for_each(|object| took[at][*object] = true),
                WaitOutcome::TimedOut => {}
                WaitOutcome::Interrupted => panic!("中断していない待機が中断で終わった"),
            }
        };
        for (op, last) in path.iter().zip((1..=path.len()).rev().map(|left| left == 1)) {
            let accepted = match *op {
                WaitOp::Wait(at, order, mode) => {
                    if want[at].is_some() {
                        false
                    } else {
                        want[at] = Some((order, mode));
                        let targets: Vec<ObjectId> = ORDERS[order].iter().map(|object| objects[*object]).collect();
                        let until = deadline[at].map_or(Deadline::Never, |nth| Deadline::At(Instant::from_nanos(nth)));
                        match k.wait(made[at], &targets, mode, until, Interruptible::No).unwrap() {
                            WaitStart::Done(outcome) => settle(at, outcome, &want, &mut queue),
                            WaitStart::Blocked => queue.push(at),
                        }
                        true
                    }
                }
                WaitOp::Signal(object) => {
                    sent[object] < MAX_SIGNALS && {
                        match k.signal(objects[object], SignalOp::Add(1)) {
                            Ok(_) => {
                                sent[object] = sent[object].checked_add(1).unwrap();
                                true
                            }
                            Err(Error::Refused(_)) => false,
                            Err(other) => panic!("通知が {other:?} で失敗した"),
                        }
                    }
                }
                WaitOp::Timeout(at) => {
                    want[at].is_some() && {
                        timeouts = timeouts.checked_add(1).unwrap();
                        assert!(!k.advance(Instant::from_nanos(timeouts)).unwrap().more());
                        true
                    }
                }
            };
            if !accepted {
                assert!(last, "受け付けたはずの操作を、やり直したら受け付けなかった: {path:?}");
                return None;
            }
            for effect in drain(&mut k) {
                let Effect::WaitDone { thread, outcome } = effect else { panic!("待機の結果ではない Effect: {effect:?}") };
                let at = made.iter().position(|known| *known == thread).unwrap();
                settle(at, outcome, &want, &mut queue);
            }
        }
        k.audit().unwrap();
        // モデルの queue と want は、実装の待機列と待機の内容そのものである。
        for (object, id) in objects.iter().enumerate() {
            let waiters: Vec<usize> = queue
                .iter()
                .copied()
                .filter(|at| want[*at].is_some_and(|(order, _)| ORDERS[order].contains(&object)))
                .collect();
            let listed: Vec<usize> = k.objects.get(*id).unwrap().waiters.iter().map(|w| made.iter().position(|t| t == w).unwrap()).collect();
            assert_eq!(listed, waiters, "資源 {} の待機列が、登録順と違う", OBJECTS[object]);
        }
        let phase: Vec<&str> = (0..3)
            .map(|at| match &k.threads.get(made[at]).unwrap().run {
                Run::Blocked(wait) => {
                    let (order, mode) = want[at].unwrap();
                    let targets: Vec<ObjectId> = ORDERS[order].iter().map(|object| objects[*object]).collect();
                    assert_eq!((wait.targets.as_slice(), wait.mode), (targets.as_slice(), mode));
                    assert!(queue.contains(&at));
                    "waiting"
                }
                Run::Servicing => {
                    assert!(!queue.contains(&at));
                    if want[at].is_some() { "done" } else { "idle" }
                }
                Run::Ready | Run::Running { .. } => panic!("待機の筋書きに無い状態"),
            })
            .collect();
        let counts = objects.map(|object| k.signal_count(object).unwrap());
        let names = |picked: &[usize]| picked.iter().map(|object| quoted(OBJECTS[*object])).collect::<Vec<_>>().join(", ");
        Some(state_text(vec![
            format!("count = [a |-> {}, b |-> {}]", counts[0], counts[1]),
            format!("sent = [a |-> {}, b |-> {}]", sent[0], sent[1]),
            format!("phase = {}", per_thread(&phase, |name| quoted(name))),
            format!(
                "want = {}",
                per_thread(&want, |want| match want {
                    None => "<<<<>>, \"Any\">>".to_owned(),
                    Some((order, mode)) => format!("<<<<{}>>, \"{mode:?}\">>", names(ORDERS[*order])),
                })
            ),
            format!("queue = <<{}>>", queue.iter().map(|at| format!("t{}", at.checked_add(1).unwrap())).collect::<Vec<_>>().join(", ")),
            format!("outcomes = {}", per_thread(&outcomes, |count| format!("{count}"))),
            format!(
                "took = {}",
                per_thread(&took, |took| {
                    let picked: Vec<usize> = (0..2).filter(|object| took[*object]).collect();
                    format!("{{{}}}", names(&picked))
                })
            ),
        ]))
    }
}

/// tla/WaitRace.tla の状態と遷移は、`Kernel` の待機と通知と期限の動きそのものである。
#[test]
fn the_wait_model_and_the_kernel_take_the_same_steps() {
    conform::<WaitRace>();
}

// ---- IoLifecycle ----

#[derive(Clone, Copy, Debug)]
enum IoOp {
    Issue,
    Cancel,
    WriteOutput,
    Publish,
    Release,
    HostTake,
    HostSeeCancel,
    HostAnswer,
    HostResend,
    Deliver,
    OwnerExit,
}

struct IoLifecycle;

impl Driven for IoLifecycle {
    type Op = IoOp;
    const NAME: &'static str = "IoLifecycle";

    fn ops() -> Vec<IoOp> {
        use IoOp::*;
        vec![Issue, Cancel, WriteOutput, Publish, Release, HostTake, HostSeeCancel, HostAnswer, HostResend, Deliver, OwnerExit]
    }

    fn run(path: &[IoOp]) -> Option<String> {
        let mut k = kernel(1);
        let p = process(&mut k);
        let [waiter]: [ThreadId; 1] = threads(&mut k, p);
        let notify = k.create_object(p, SignalSpec::new(SignalMode::Keep, 0, 1)).unwrap();
        let io = k.prepare_io(p, IoLinks::NONE.with_notify(notify)).unwrap();
        assert_eq!(trap(&mut k), waiter);
        let started = k.wait(waiter, &[notify], WaitMode::Any, Deadline::Never, Interruptible::No);
        assert_eq!(started, Ok(WaitStart::Blocked));
        // カーネルの外の世界 (ホストと Personality) の状態。モデルの同じ名前の変数である。
        let (mut submit, mut cancel) = (false, false);
        let mut host = "idle";
        let mut msgs = 0u32;
        let (mut resent, mut written, mut woken) = (false, false, false);
        let mut settled = 0u32;
        let refused = |result: Result<(), Error>| match result {
            Ok(()) => true,
            Err(Error::Refused(_) | Error::Stale(_)) => false,
            Err(other) => panic!("I/O の操作が {other:?} で失敗した"),
        };
        for (op, last) in path.iter().zip((1..=path.len()).rev().map(|left| left == 1)) {
            let accepted = match op {
                IoOp::Issue => refused(k.issue_io(io)),
                IoOp::Cancel => refused(k.cancel_io(io).map(|reply| match reply {
                    CancelReply::Withdrawn => settled = settled.checked_add(1).unwrap(),
                    CancelReply::Requested | CancelReply::AlreadyRequested => {}
                })),
                // 確定した結果の出力を書くのは Personality。確定していて、持ち主が生きている間だけ書く。
                IoOp::WriteOutput => {
                    let can = !written && k.io_phase(io) == Ok(IoPhase::Settled);
                    written |= can;
                    can
                }
                // 出力を書く前に公開しないのは、Personality の規則。
                IoOp::Publish => written && refused(k.publish_io(io).map(|_| ())),
                IoOp::Release => refused(k.release_io(io)),
                IoOp::HostTake => {
                    let can = submit && host == "idle";
                    if can {
                        (submit, host) = (false, "working");
                    }
                    can
                }
                IoOp::HostSeeCancel => core::mem::take(&mut cancel),
                IoOp::HostAnswer => {
                    let can = host == "working";
                    if can {
                        (host, msgs) = ("answered", msgs.checked_add(1).unwrap());
                    }
                    can
                }
                IoOp::HostResend => {
                    let can = host == "answered" && !resent;
                    if can {
                        (resent, msgs) = (true, msgs.checked_add(1).unwrap());
                    }
                    can
                }
                IoOp::Deliver => {
                    msgs > 0 && {
                        msgs = msgs.checked_sub(1).unwrap();
                        let completion = Completion::new(k.instance(), CompletionKind::Io { io, outcome: DONE });
                        if refused(k.complete(completion).map(|_| ())) {
                            settled = settled.checked_add(1).unwrap();
                        }
                        true
                    }
                }
                IoOp::OwnerExit => refused(k.exit_process(p, CODE)),
            };
            if !accepted {
                assert!(last, "受け付けたはずの操作を、やり直したら受け付けなかった: {path:?}");
                return None;
            }
            for effect in drain(&mut k) {
                match effect {
                    Effect::IoSubmit { io: which } => {
                        assert_eq!(which, io);
                        assert!(!core::mem::replace(&mut submit, true), "発行を二度ホストへ出した");
                    }
                    Effect::IoCancel { io: which } => {
                        assert_eq!(which, io);
                        cancel = true;
                    }
                    Effect::WaitDone { thread, outcome } => {
                        assert_eq!((thread, outcome), (waiter, WaitOutcome::Signaled { index: 0 }));
                        assert!(!core::mem::replace(&mut woken, true), "待機を二度解除した");
                    }
                    Effect::ThreadExited { .. }
                    | Effect::ProcessExited { .. }
                    | Effect::ProcessReclaimed { .. }
                    | Effect::ObjectReclaimed { .. } => {}
                    Effect::Interrupt { .. } | Effect::BackingReclaimed { .. } => panic!("筋書きに無い Effect: {effect:?}"),
                }
            }
        }
        k.audit().unwrap();
        let state = match k.ios.get(io).map(|entry| k.io_phase(io).unwrap_or_else(|error| panic!("{error:?} {:?}", entry.process))) {
            Some(IoPhase::Prepared) => "Prepared",
            Some(IoPhase::Issued) => "Issued",
            Some(IoPhase::CancelRequested) => "CancelRequested",
            Some(IoPhase::Settled) => "Settled",
            Some(IoPhase::Published) => "Published",
            None => "Gone",
        };
        let owner = if k.process_phase(p) == Ok(ProcessPhase::Alive) { "alive" } else { "exited" };
        let effects: Vec<String> = [(cancel, "Cancel"), (submit, "Submit")].iter().filter(|(on, _)| *on).map(|(_, name)| quoted(name)).collect();
        Some(state_text(vec![
            format!("state = {}", quoted(state)),
            format!("settled = {settled}"),
            format!("effects = {{{}}}", effects.join(", ")),
            format!("host = {}", quoted(host)),
            format!("msgs = {msgs}"),
            format!("resent = {}", truth(resent)),
            format!("written = {}", truth(written)),
            format!("woken = {}", truth(woken)),
            format!("owner = {}", quoted(owner)),
        ]))
    }
}

/// tla/IoLifecycle.tla の状態と遷移は、`Kernel` の I/O 要求の動きそのものである。
#[test]
fn the_io_model_and_the_kernel_take_the_same_steps() {
    conform::<IoLifecycle>();
}

// ---- ProcessExit ----

#[derive(Clone, Copy, Debug)]
enum Back {
    Yielded,
    BudgetExhausted,
    Interrupted,
    Trap,
    Exited,
}

#[derive(Clone, Copy, Debug)]
enum ExitOp {
    Grant(usize),
    /// 実行権を、この理由で返す。
    Return(usize, Back),
    Continue(usize),
    Block(usize),
    Wake(usize),
    Terminate(usize),
    IoPrepare,
    IoIssue,
    IoCancel,
    IoPublish,
    IoRelease,
    ExitProcess,
    HostComplete,
}

struct ProcessExit;

impl Driven for ProcessExit {
    type Op = ExitOp;
    const NAME: &'static str = "ProcessExit";

    fn ops() -> Vec<ExitOp> {
        use ExitOp::*;
        let mut ops = vec![IoPrepare, IoIssue, IoCancel, IoPublish, IoRelease, ExitProcess, HostComplete];
        for thread in 0..3 {
            ops.extend([Grant(thread), Continue(thread), Block(thread), Wake(thread), Terminate(thread)]);
            for back in [Back::Yielded, Back::BudgetExhausted, Back::Interrupted, Back::Trap, Back::Exited] {
                ops.push(Return(thread, back));
            }
        }
        ops
    }

    fn run(path: &[ExitOp]) -> Option<String> {
        let mut k = kernel(3);
        let p = process(&mut k);
        let made: [ThreadId; 3] = threads(&mut k, p);
        // スレッドごとの、待つ相手。プロセスが持つ参照でもある。
        let gates = [(); 3].map(|()| k.create_object(p, SignalSpec::new(SignalMode::Consume, 0, 1)).unwrap());
        drain(&mut k);
        let mut permits: [Option<RunPermit>; 3] = [None, None, None];
        let mut io: Option<IoId> = None;
        let mut exits = 0u32;
        let (mut late_grant, mut drained) = (false, false);
        let refused = |result: Result<(), Error>| match result {
            Ok(()) => true,
            Err(Error::Refused(_) | Error::Stale(_)) => false,
            Err(other) => panic!("操作が {other:?} で失敗した"),
        };
        for (op, last) in path.iter().zip((1..=path.len()).rev().map(|left| left == 1)) {
            // Personality が I/O を扱うのは、サービス中のスレッドがあるときだけ。
            let serviced = made.iter().any(|thread| k.thread_phase(*thread) == Ok(ThreadPhase::Servicing));
            let accepted = match *op {
                ExitOp::Grant(at) => {
                    let alive = k.process_phase(p) == Ok(ProcessPhase::Alive);
                    match k.grant_thread(WorkerId::new(u32::try_from(at).unwrap()), made[at]) {
                        Ok(permit) => {
                            permits[at] = Some(permit);
                            late_grant |= !alive;
                            true
                        }
                        Err(Error::Refused(_) | Error::Stale(_)) => false,
                        Err(other) => panic!("実行権を貸すのが {other:?} で失敗した"),
                    }
                }
                ExitOp::Return(at, back) => match permits[at].take() {
                    None => false,
                    Some(permit) => {
                        let (stop, consumed) = match back {
                            Back::Yielded => (Stop::Yielded, Budget::new(1)),
                            Back::BudgetExhausted => (Stop::BudgetExhausted, permit.budget()),
                            Back::Interrupted => (Stop::Interrupted, Budget::new(1)),
                            Back::Trap => (Stop::Trap, Budget::new(1)),
                            Back::Exited => (Stop::Exited(CODE), Budget::new(1)),
                        };
                        match k.relinquish(permit, stop, consumed) {
                            Ok(_) => true,
                            Err(NotRelinquished::Rejected(_, Error::Refused(_))) => false,
                            Err(other) => panic!("実行権を返すのが {other:?} で失敗した"),
                        }
                    }
                },
                ExitOp::Continue(at) => refused(k.continue_thread(made[at])),
                ExitOp::Block(at) => refused(
                    k.wait(made[at], &[gates[at]], WaitMode::Any, Deadline::Never, Interruptible::No)
                        .map(|started| assert_eq!(started, WaitStart::Blocked)),
                ),
                // 待機中のスレッドだけを起こす。待っていない相手への通知は、この筋書きでは行わない。
                ExitOp::Wake(at) => {
                    k.thread_phase(made[at]) == Ok(ThreadPhase::Blocked) && refused(k.signal(gates[at], SignalOp::Add(1)).map(|_| ()))
                }
                ExitOp::Terminate(at) => refused(k.terminate_thread(made[at], CODE)),
                ExitOp::IoPrepare => {
                    serviced && io.is_none() && {
                        match k.prepare_io(p, IoLinks::NONE) {
                            Ok(made) => {
                                io = Some(made);
                                true
                            }
                            Err(Error::Refused(_)) => false,
                            Err(other) => panic!("I/O の準備が {other:?} で失敗した"),
                        }
                    }
                }
                ExitOp::IoIssue => serviced && io.is_some_and(|io| refused(k.issue_io(io))),
                ExitOp::IoCancel => serviced && io.is_some_and(|io| refused(k.cancel_io(io).map(|_| ()))),
                ExitOp::IoPublish => serviced && io.is_some_and(|io| refused(k.publish_io(io).map(|_| ()))),
                ExitOp::IoRelease => {
                    let released = serviced && io.is_some_and(|io| refused(k.release_io(io)));
                    if released {
                        io = None;
                    }
                    released
                }
                ExitOp::ExitProcess => refused(k.exit_process(p, CODE)),
                ExitOp::HostComplete => io.is_some_and(|io| {
                    let completion = Completion::new(k.instance(), CompletionKind::Io { io, outcome: DONE });
                    refused(k.complete(completion).map(|settled| match settled {
                        IoSettled::Held(_) => {}
                        IoSettled::Orphaned(_) => drained = true,
                    }))
                }),
            };
            if !accepted {
                assert!(last, "受け付けたはずの操作を、やり直したら受け付けなかった: {path:?}");
                return None;
            }
            for effect in drain(&mut k) {
                if matches!(effect, Effect::ProcessExited { .. }) {
                    exits = exits.checked_add(1).unwrap();
                }
            }
        }
        k.audit().unwrap();
        let entry = |at: usize| k.threads.get(made[at]);
        let run: Vec<&str> = (0..3)
            .map(|at| match entry(at).map(|thread| &thread.run) {
                None => "Gone",
                Some(Run::Ready) => "Ready",
                Some(Run::Running { .. }) => "Running",
                Some(Run::Servicing) => "Servicing",
                Some(Run::Blocked(_)) => "Blocked",
            })
            .collect();
        let kill: Vec<bool> = (0..3).map(|at| entry(at).is_some_and(|thread| thread.kill.is_some())).collect();
        let asked: Vec<bool> =
            (0..3).map(|at| entry(at).is_some_and(|thread| matches!(thread.run, Run::Running { interrupted: true, .. }))).collect();
        let owner = k.processes.get(p);
        let life = match owner.map(|owner| owner.life) {
            Some(process::Life::Alive) => "Alive",
            Some(process::Life::Exiting(_)) => "Exiting",
            Some(process::Life::Draining(_)) => "Draining",
            None => "Reclaimed",
        };
        let handles = u32::from(owner.is_some_and(|owner| !owner.objects.is_empty() || !owner.backings.is_empty()));
        let io_state = match io.map(|io| k.ios.get(io).map(|_| k.io_phase(io).unwrap())) {
            None => "None",
            Some(None) => "Gone",
            Some(Some(IoPhase::Prepared)) => "Prepared",
            Some(Some(IoPhase::Issued)) => "Issued",
            Some(Some(IoPhase::CancelRequested)) => "CancelRequested",
            Some(Some(IoPhase::Settled)) => "Settled",
            Some(Some(IoPhase::Published)) => "Published",
        };
        Some(state_text(vec![
            format!("run = {}", per_thread(&run, |name| quoted(name))),
            format!("kill = {}", per_thread(&kill, |on| truth(*on).to_owned())),
            format!("asked = {}", per_thread(&asked, |on| truth(*on).to_owned())),
            format!("life = {}", quoted(life)),
            format!("io = {}", quoted(io_state)),
            format!("handles = {handles}"),
            format!("exits = {exits}"),
            format!("lateGrant = {}", truth(late_grant)),
            format!("drained = {}", truth(drained)),
        ]))
    }
}

/// tla/ProcessExit.tla の状態と遷移は、`Kernel` のプロセスの終了処理の動きそのものである。
#[test]
fn the_exit_model_and_the_kernel_take_the_same_steps() {
    conform::<ProcessExit>();
}

#[test]
fn a_dumped_graph_is_read_with_its_variables_in_name_order() {
    let text = "strict digraph DiskGraph {\n\
        nodesep=0.35;\n\
        -5 [label=\"/\\\\ b = ( t1 :> \\\"x\\\" @@\\n  t2 :> 0 )\\n/\\\\ a = 1\",style = filled]\n\
        -5 -> 7 [label=\"\",color=\"black\",fontcolor=\"black\"];\n\
        7 [label=\"/\\\\ b = \\\"y\\\"\\n/\\\\ a = 1\"];\n\
        7 -> 7 [label=\"\",color=\"black\",fontcolor=\"black\"];\n\
        {rank = same; -5;}\n}\n";
    let graph = read_graph(text);
    assert_eq!(graph.initial, "a=1\nb=(t1:>\"x\"@@t2:>0)", "折られた値は一行に詰める");
    assert_eq!(graph.ids.keys().map(String::as_str).collect::<Vec<_>>(), ["a=1\nb=\"y\"", "a=1\nb=(t1:>\"x\"@@t2:>0)"]);
    assert_eq!(graph.edges.len(), 1, "同じ状態への遷移は数えない");
}
