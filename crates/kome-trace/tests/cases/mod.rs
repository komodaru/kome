//! 記録と再生の試験。
//!
//! - どの操作の列も、記録して再生すると同じ返答と同じ状態になる (proptest)。
//! - 全ての操作が、記録と再生の両方を通る。
//! - 記録を一か所壊すと、再生は必ず止まる。書き換えは照合値で、内容のずれは返答の行で見つかる。
//! - 実装の側が違う (上限が違う) と、返答が違った最初の行で止まる。

use kome::{
    AddrWidth, BackingId, BackingOffset, Budget, ChannelId, ChannelSpec, Completion, CompletionKind, Deadline, Duration, End,
    ExitCode, GuestAddr, GuestRange, HostError, Input, InstanceId, Interruptible, IoId, IoLinks, IoOutcome, Limits,
    MapSource, MemFault, MemFaultKind, ObjectId, PageSize, Parcel, Period, Priority, ProcessId, Prot, Quota, Reply, RunPermit,
    SignalMode, SignalOp, SignalSpec, Start, Stop, ThreadId, TimerId, WaitMode, WorkerId,
};
use kome_trace::{Recording, Replayed, Sealer, Stopped, contents, replay};
use proptest::prelude::*;

const PAGE: u64 = 0x1000;
const INSTANCE: InstanceId = InstanceId::new(7);
/// [`Input`] の項目の数。`step` が全ての項目を作る。
const KINDS: u64 = 42;

fn limits() -> Limits {
    Limits::DEFAULT.with_workers(2).with_threads(24).with_effects(64)
}

/// 返答から集めた、次の操作の引数にする値。
#[derive(Default)]
struct Pools {
    processes: Vec<ProcessId>,
    threads: Vec<ThreadId>,
    objects: Vec<ObjectId>,
    backings: Vec<BackingId>,
    timers: Vec<TimerId>,
    ios: Vec<IoId>,
    channels: Vec<ChannelId>,
    permits: Vec<RunPermit>,
}

fn pick<T: Copy>(pool: &[T], seed: u64) -> Option<T> {
    let at = usize::try_from(seed).unwrap() % pool.len().max(1);
    pool.get(at).copied()
}

/// 一つの操作を作って適用する。引数が揃わなければ、何もせず `None`。
fn step(r: &mut Recording, pools: &mut Pools, (kind, a, b, c): (u64, u64, u64, u64)) -> Option<()> {
    let range = GuestRange::new(GuestAddr::new(PAGE * (1 + a % 6)), PAGE * (1 + b % 2)).unwrap();
    let prot = [Prot::NONE, Prot::READ, Prot::READ_WRITE, Prot::READ_EXECUTE][usize::try_from(c % 4).unwrap()];
    let code = ExitCode::new(c % 5);
    let now = r.kernel().now();
    let later = now.checked_add(Duration::from_nanos(1 + c % 900)).unwrap();
    let process = pick(&pools.processes, a);
    let thread = pick(&pools.threads, a);
    let object = pick(&pools.objects, b);
    let backing = pick(&pools.backings, b);
    let timer = pick(&pools.timers, a);
    let io = pick(&pools.ios, a);
    let channel = pick(&pools.channels, b);
    let end = if c % 2 == 0 { End::Send } else { End::Receive };
    let bytes = [u8::try_from(a % 251).unwrap(); 12];
    let mut buffer = [0u8; 16];
    let targets: Vec<ObjectId> = [object, pick(&pools.objects, c)].into_iter().flatten().take(usize::try_from(1 + c % 2).unwrap()).collect();
    let input = match kind % KINDS {
        0 => Input::CreateProcess(if a % 2 == 0 { AddrWidth::Bits64 } else { AddrWidth::Bits32 }, PageSize::new(PAGE).unwrap()),
        1 if c % 7 == 0 => Input::ExitProcess(process?, code),
        1 => return None,
        2 => Input::SetQuota(process?, Quota::UNLIMITED.with_threads(u32::try_from(2 + b % 6).unwrap()).with_mapped_bytes(PAGE * (1 + c % 9))),
        3 => Input::CreateThread(process?, if c % 4 == 0 { Start::Suspended } else { Start::Ready }),
        4 => Input::ContinueThread(thread?),
        5 => Input::SuspendThread(thread?),
        6 => Input::UnsuspendThread(thread?),
        7 if c % 5 == 0 => Input::TerminateThread(thread?, code),
        7 => return None,
        8 => Input::Grant(WorkerId::new(u32::try_from(a % 3).unwrap())),
        9 => Input::GrantThread(WorkerId::new(u32::try_from(b % 2).unwrap()), thread?),
        10 => Input::PreemptThread(thread?),
        11 => Input::SetPriority(thread?, Priority::new(u8::try_from(b % 256).unwrap())),
        12 => {
            if pools.permits.is_empty() {
                return None;
            }
            let permit = pools.permits.swap_remove(usize::try_from(a).unwrap() % pools.permits.len());
            let stop = match b % 6 {
                0 => Stop::BudgetExhausted,
                1 => Stop::Yielded,
                2 => Stop::Interrupted,
                3 => Stop::Fault(MemFault::new(GuestAddr::new(c), [MemFaultKind::Unmapped, MemFaultKind::Reserved, MemFaultKind::Denied][usize::try_from(c % 3).unwrap()])),
                4 => Stop::Exited(code),
                _ => Stop::Trap,
            };
            Input::Relinquish(permit, stop, Budget::new(if c % 2 == 0 { 0 } else { 1_000_000 }))
        }
        13 => Input::CreateObject(process?, SignalSpec::new(if a % 2 == 0 { SignalMode::Keep } else { SignalMode::Consume }, b % 2, 1 + c % 3)),
        14 => Input::RetainObject(process?, object?),
        15 => Input::ReleaseObject(process?, object?),
        16 => Input::Signal(object?, if c % 2 == 0 { SignalOp::Add(1) } else { SignalOp::Set(c % 3) }),
        17 => Input::Wait(
            thread?,
            &targets,
            if c % 2 == 0 { WaitMode::Any } else { WaitMode::All },
            if c % 3 == 0 { Deadline::Never } else { Deadline::At(later) },
            if b % 2 == 0 { Interruptible::Yes } else { Interruptible::No },
        ),
        18 => Input::InterruptWait(thread?),
        19 => Input::CreateTimer(process?, object?),
        20 => Input::ArmTimer(timer?, later, if b % 2 == 0 { None } else { Period::new(Duration::from_nanos(100_000 + c)) }),
        21 => Input::DisarmTimer(timer?),
        22 if c % 3 == 0 => Input::DropTimer(timer?),
        22 => return None,
        23 => Input::Advance(later),
        24 => {
            let mut links = IoLinks::NONE;
            if let (0, Some(target)) = (b % 2, object) {
                links = links.with_target(target);
            }
            if let (0, Some(notify)) = (c % 2, pick(&pools.objects, c)) {
                links = links.with_notify(notify);
            }
            Input::PrepareIo(process?, links)
        }
        25 => Input::IssueIo(io?),
        26 => Input::CancelIo(io?),
        27 => {
            let outcome = match b % 3 {
                0 => IoOutcome::Done { transferred: c },
                1 => IoOutcome::Failed { error: HostError::new(u32::try_from(c % 99).unwrap()), transferred: b },
                _ => IoOutcome::Cancelled { transferred: c },
            };
            // たまに、別の実行インスタンス宛ての応答を渡す。
            let instance = if c % 11 == 0 { InstanceId::new(8) } else { INSTANCE };
            Input::Complete(Completion::new(instance, CompletionKind::Io { io: io?, outcome }))
        }
        28 => Input::PublishIo(io?),
        29 => Input::ReleaseIo(io?),
        30 => Input::CreateBacking(process?, PAGE * (1 + c % 3)),
        31 => Input::RetainBacking(process?, backing?),
        32 => Input::ReleaseBacking(process?, backing?),
        33 => {
            let source = match (c % 3, backing) {
                (0, _) | (_, None) => MapSource::Reserved,
                (_, Some(backing)) => MapSource::Backing { backing, offset: BackingOffset::new(PAGE * (c % 2)) },
            };
            Input::Map(process?, range, source, prot)
        }
        34 => Input::Unmap(process?, range),
        35 => Input::Protect(process?, range, prot),
        36 => Input::CreateChannel(process?, if c % 2 == 0 { ChannelSpec::messages(2, 16) } else { ChannelSpec::stream(20) }),
        37 => Input::RetainChannel(process?, channel?, end),
        38 => Input::ReleaseChannel(process?, channel?, end),
        39 => {
            let parcel = match (c % 4, object, backing) {
                (0, Some(object), _) => Some(Parcel::Object(object)),
                (1, _, Some(backing)) => Some(Parcel::Backing(backing)),
                _ => None,
            };
            let len = usize::try_from(c % 13).unwrap();
            Input::Send(process?, channel?, &bytes[..len], parcel)
        }
        40 => Input::Receive(process?, channel?, &mut buffer),
        _ => Input::PollEffect,
    };
    match r.apply(input) {
        Ok(Reply::Process(process, exit)) => {
            pools.processes.push(process);
            pools.objects.push(exit);
        }
        Ok(Reply::Thread(thread, exit)) => {
            pools.threads.push(thread);
            pools.objects.push(exit);
        }
        Ok(Reply::Object(object)) => pools.objects.push(object),
        Ok(Reply::Backing(backing)) => pools.backings.push(backing),
        Ok(Reply::Timer(timer)) => pools.timers.push(timer),
        Ok(Reply::Io(io)) => pools.ios.push(io),
        Ok(Reply::Channel(made)) => {
            pools.channels.push(made.channel());
            pools.objects.extend([made.readable(), made.writable()]);
        }
        Ok(Reply::Permit(Some(permit)) | Reply::Returned(permit, _)) => pools.permits.push(permit),
        Ok(_) | Err(_) => {}
    }
    Some(())
}

/// 操作の列を記録する。記録の文章と、記録した状態を返す。
fn record(ops: &[(u64, u64, u64, u64)]) -> (String, Recording) {
    let mut recording = Recording::new(INSTANCE, limits()).unwrap();
    let mut pools = Pools::default();
    // 最初にプロセスとスレッドを作っておく (引数の無い列ばかりにならないように)。
    for op in [(0, 0, 0, 0), (3, 0, 0, 1), (3, 0, 0, 1), (30, 0, 0, 0), (36, 0, 0, 0), (13, 0, 1, 0)].iter().chain(ops) {
        // 引数が揃わない操作は飛ばす。
        let _ = step(&mut recording, &mut pools, *op);
    }
    (recording.take_text(), recording)
}

fn ops() -> impl Strategy<Value = Vec<(u64, u64, u64, u64)>> {
    prop::collection::vec((0..KINDS, any::<u64>(), any::<u64>(), any::<u64>()), 0..160)
}

/// 決まった種から作る操作の列。
fn seeded(seed: u64, len: usize) -> Vec<(u64, u64, u64, u64)> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).wrapping_add(1);
    let mut next = || {
        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        state >> 20
    };
    (0..len).map(|_| (next() % KINDS, next(), next(), next())).collect()
}

/// 内容の列から、照合値を付け直した記録を作る。
fn sealed(lines: &[String]) -> String {
    let mut sealer = Sealer::default();
    for line in lines {
        sealer.line(line);
    }
    sealer.take()
}

fn replayed(text: &str) -> Replayed {
    replay(text).unwrap_or_else(|stopped| panic!("{stopped:?}\n{text}"))
}

proptest! {
    #![proptest_config(ProptestConfig { cases: if cfg!(miri) { 2 } else { 192 }, ..ProptestConfig::default() })]

    #[test]
    fn a_recording_replays_to_the_same_replies_and_state(ops in ops()) {
        let (text, recording) = record(&ops);
        let again = replayed(&text);
        prop_assert_eq!(&again.kernel, recording.kernel());
        prop_assert_eq!(again.unanswered, None);
        again.kernel.audit().unwrap();
        prop_assert_eq!(u64::try_from(text.lines().filter(|line| line.starts_with("> ")).count()).unwrap(), again.steps);
    }

    /// 文章を一文字でも書き換えるか、行を抜く・重ねる・入れ替えると、再生は必ず止まる。
    /// 末尾の行を抜いた記録だけは、途中で終わった記録として読める。
    #[test]
    fn any_damage_to_the_text_stops_the_replay(ops in ops(), at in any::<prop::sample::Index>(), how in 0..5u8, with in 0x21u8..0x7f) {
        let (text, _) = record(&ops);
        let lines: Vec<&str> = text.lines().collect();
        let line = at.index(lines.len());
        let mut damaged: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
        match how {
            0 => {
                let column = at.index(damaged[line].len());
                prop_assume!(damaged[line].is_char_boundary(column) && damaged[line].is_char_boundary(column + 1));
                prop_assume!(damaged[line].as_bytes()[column] != with);
                damaged[line].replace_range(column..=column, &char::from(with).to_string());
            }
            1 => {
                damaged.remove(line);
            }
            2 => damaged.insert(line, lines[line].to_owned()),
            3 => {
                prop_assume!(line + 1 < damaged.len() && damaged[line] != damaged[line + 1]);
                damaged.swap(line, line + 1);
            }
            _ => {
                prop_assume!(line + 2 < damaged.len());
                damaged.drain(line..line + 2);
            }
        }
        let cut_at_the_end = how == 1 && line + 1 == lines.len();
        let outcome = replay(&(damaged.join("\n") + "\n"));
        if cut_at_the_end {
            prop_assert!(outcome.is_ok_and(|replayed| replayed.unanswered.is_some()));
        } else {
            prop_assert!(matches!(outcome, Err(Stopped::Malformed { .. })), "{:?}", outcome.map(|replayed| replayed.steps));
        }
    }

    /// 返答を書き換えた記録 (照合値は付け直す) は、その行で不一致として止まる。
    #[test]
    fn a_changed_reply_is_a_mismatch_at_that_line(ops in ops(), at in any::<prop::sample::Index>()) {
        let (text, _) = record(&ops);
        let mut lines: Vec<String> = contents(&text).unwrap().into_iter().map(str::to_owned).collect();
        let replies: Vec<usize> = (0..lines.len()).filter(|line| lines[*line].starts_with("< ")).collect();
        let line = replies[at.index(replies.len())];
        let original = lines[line].clone();
        lines[line] = original.replacen("effects=", "effects=1", 1);
        let stopped = replay(&sealed(&lines)).map(|replayed| replayed.steps).unwrap_err();
        prop_assert_eq!(stopped, Stopped::Mismatch { line: line + 1, recorded: lines[line].clone(), replayed: original });
    }

    /// 資源を作った操作を一つ抜いた記録 (照合値は付け直す) は、その後の最初の返答までに止まる。
    #[test]
    fn a_missing_input_stops_the_replay(ops in ops(), at in any::<prop::sample::Index>()) {
        let (text, _) = record(&ops);
        let mut lines: Vec<String> = contents(&text).unwrap().into_iter().map(str::to_owned).collect();
        let made = |reply: &str| ["Ok(Process(", "Ok(Thread(", "Ok(Object(", "Ok(Backing(", "Ok(Timer(", "Ok(Io(", "Ok(Channel("].iter().any(|kind| reply.contains(kind));
        let creations: Vec<usize> = (0..lines.len().saturating_sub(2)).filter(|line| lines[*line].starts_with("< ") && made(&lines[*line])).collect();
        prop_assume!(!creations.is_empty());
        let reply = creations[at.index(creations.len())];
        lines.drain(reply - 1..=reply);
        match replay(&sealed(&lines)).map(|replayed| replayed.steps) {
            // 抜いた操作の次の返答で、生きている資源の数が合わなくなる。
            Err(Stopped::Mismatch { line, .. }) => prop_assert_eq!(line, reply + 1),
            // 次の入力が、抜いた操作の作った資源を番号で指していた。
            Err(Stopped::Malformed { line, .. }) => prop_assert_eq!(line, reply),
            other => prop_assert!(false, "{:?}", other),
        }
    }

    /// 記録として読めない文章でも、再生は落ちずに止まる。
    #[test]
    fn replay_never_panics_on_arbitrary_lines(lines in prop::collection::vec("[ -~]{0,60}", 0..12), valid in 0..8usize) {
        let (text, _) = record(&seeded(3, 40));
        let mut all: Vec<String> = contents(&text).unwrap().into_iter().take(3 + valid).map(str::to_owned).collect();
        all.extend(lines.into_iter().map(|line| format!("> {line}")));
        let _ = replay(&sealed(&all));
        let _ = replay(&all.join("\n"));
    }
}

/// 全ての操作が、記録と再生を通る。成功する場合も、その操作ごとに少なくとも一度は通る。
#[test]
fn every_operation_is_recorded_and_replayed() {
    let names = [
        "CreateProcess", "ExitProcess", "SetQuota", "CreateThread", "ContinueThread", "SuspendThread", "UnsuspendThread",
        "TerminateThread", "Grant", "GrantThread", "PreemptThread", "SetPriority", "Relinquish", "CreateObject", "RetainObject",
        "ReleaseObject", "Signal", "Wait", "InterruptWait", "CreateTimer", "ArmTimer", "DisarmTimer", "DropTimer", "Advance",
        "PrepareIo", "IssueIo", "CancelIo", "Complete", "PublishIo", "ReleaseIo", "CreateBacking", "RetainBacking",
        "ReleaseBacking", "Map", "Unmap", "Protect", "CreateChannel", "RetainChannel", "ReleaseChannel", "Send", "Receive",
        "PollEffect",
    ];
    assert_eq!(u64::try_from(names.len()).unwrap(), KINDS);
    let mut succeeded = std::collections::BTreeSet::new();
    let mut seen = std::collections::BTreeSet::new();
    let seeds = if cfg!(miri) { 2 } else { 60 };
    for seed in 0..seeds {
        let (text, recording) = record(&seeded(seed, 400));
        assert_eq!(&replayed(&text).kernel, recording.kernel(), "seed {seed}");
        let lines = contents(&text).unwrap();
        for pair in lines.windows(2) {
            let Some(input) = pair[0].strip_prefix("> ") else { continue };
            let name = input.split(' ').next().unwrap();
            seen.insert(name.to_owned());
            if pair[1].contains(" Ok(") {
                succeeded.insert(name.to_owned());
            }
        }
        // 取り出した Effect と、受信したバイト列も記録に入っている。
        if seed == 0 {
            assert!(lines.iter().any(|line| line.contains("Ok(Effect(Some(")), "{text}");
        }
    }
    if cfg!(miri) {
        return;
    }
    let missing: Vec<&str> = names.iter().copied().filter(|name| !succeeded.contains(*name)).collect();
    assert_eq!(missing, Vec::<&str>::new(), "成功した記録の無い操作 (記録に現れた操作: {})", seen.len());
}

#[test]
fn received_bytes_and_parcels_are_part_of_the_record() {
    let mut r = Recording::new(INSTANCE, limits()).unwrap();
    let Ok(Reply::Process(p, _)) = r.apply(Input::CreateProcess(AddrWidth::Bits64, PageSize::new(PAGE).unwrap())) else { panic!() };
    let Ok(Reply::Channel(made)) = r.apply(Input::CreateChannel(p, ChannelSpec::messages(2, 16))) else { panic!() };
    let Ok(Reply::Object(carried)) = r.apply(Input::CreateObject(p, SignalSpec::new(SignalMode::Keep, 0, 1))) else { panic!() };
    r.apply(Input::Send(p, made.channel(), b"kome", Some(Parcel::Object(carried)))).unwrap();
    let mut buffer = [0u8; 16];
    r.apply(Input::Receive(p, made.channel(), &mut buffer)).unwrap();
    assert_eq!(&buffer[..4], b"kome");
    let text = r.take_text();
    let lines = contents(&text).unwrap();
    assert_eq!(lines[9], "> Send p0 c0 6b6f6d65 Object o3");
    assert_eq!(lines[11], "> Receive p0 c0 16");
    assert!(lines[12].ends_with(" | 6b6f6d65"), "{}", lines[12]);
    // 受信したバイト列だけを変えた記録は、不一致になる。
    let mut changed: Vec<String> = lines.iter().map(|line| (*line).to_owned()).collect();
    changed[12] = changed[12].replace("6b6f6d65", "6b6f6d66");
    assert!(matches!(replay(&sealed(&changed)), Err(Stopped::Mismatch { line: 13, .. })));
    assert_eq!(&replayed(&text).kernel, r.kernel());
}

#[test]
fn a_different_implementation_stops_at_the_first_different_reply() {
    // 同じ入力列を、スレッドの上限が小さい状態で再生する。上限に当たる最初の返答で止まる。
    let (text, _) = record(&seeded(1, 300));
    let mut lines: Vec<String> = contents(&text).unwrap().into_iter().map(str::to_owned).collect();
    assert!(lines[2].contains(" threads=24 "));
    lines[2] = lines[2].replace(" threads=24 ", " threads=2 ");
    let Err(Stopped::Mismatch { line, recorded, replayed }) = replay(&sealed(&lines)) else { panic!("止まらなかった") };
    assert!(recorded.contains("Ok(Thread("), "{recorded}");
    assert!(replayed.ends_with("Err(Exhausted(Threads))"), "{replayed}");
    // 止まったのは、三つ目のスレッドを作った返答である。
    let third = (0..lines.len()).filter(|at| lines[*at].contains("Ok(Thread(")).nth(2).unwrap();
    assert_eq!(line, third + 1);
}

#[test]
fn an_input_the_kernel_never_issued_is_recorded_but_not_replayed() {
    // 別の実行インスタンスの ID は、番号で書けない。記録には残り、再生はそこで止まる。
    let mut other = Recording::new(InstanceId::new(9), limits()).unwrap();
    let Ok(Reply::Process(foreign, _)) = other.apply(Input::CreateProcess(AddrWidth::Bits64, PageSize::new(PAGE).unwrap())) else { panic!() };
    let Ok(Reply::Thread(_, _)) = other.apply(Input::CreateThread(foreign, Start::Ready)) else { panic!() };
    let Ok(Reply::Permit(Some(permit))) = other.apply(Input::Grant(WorkerId::new(0))) else { panic!() };
    let mut r = Recording::new(INSTANCE, limits()).unwrap();
    r.apply(Input::PollEffect).unwrap();
    assert_eq!(r.apply(Input::CreateThread(foreign, Start::Ready)), Err(kome::Error::Foreign));
    let text = r.take_text();
    assert!(matches!(replay(&text), Err(Stopped::NotReplayable { line: 6, .. })), "{text}");
    // 別の実行インスタンスの実行権も同じ。
    assert!(matches!(r.apply(Input::Relinquish(permit, Stop::Trap, Budget::new(0))), Ok(Reply::Returned(_, kome::Error::Foreign))));
    let rest = r.take_text();
    assert!(rest.starts_with("? Relinquish("), "{rest}");
    // 取り出した文章を順に繋げたものが、記録の全体である。
    assert!(matches!(replay(&(text.clone() + &rest)), Err(Stopped::NotReplayable { line: 6, .. })));
}

#[test]
fn a_record_cut_before_a_reply_reports_what_the_input_did() {
    let (text, _) = record(&seeded(2, 50));
    let lines: Vec<&str> = text.lines().collect();
    let cut = lines[..lines.len() - 1].join("\n");
    let again = replayed(&cut);
    assert_eq!(again.unanswered.as_deref(), contents(&text).unwrap().last().copied());
    // 形式の版、実行インスタンス、上限の行が欠けた記録は読めない。
    for keep in 0..3 {
        let head = lines[..keep].join("\n");
        assert!(matches!(replay(&head), Err(Stopped::Malformed { .. })), "{keep}");
    }
    // 上限が一つ欠けた記録、二度ある記録も読めない。
    let mut short: Vec<String> = contents(&text).unwrap().into_iter().map(str::to_owned).collect();
    let full = short[2].clone();
    short[2] = full.replace(" workers=2", "");
    assert_eq!(replay(&sealed(&short)).map(|replayed| replayed.steps), Err(Stopped::Malformed { line: 3, why: "上限の行が読めない".to_owned() }));
    short[2] = full + " workers=2";
    assert!(matches!(replay(&sealed(&short)), Err(Stopped::Malformed { line: 3, .. })));
    // 照合値は、書いたときの綴りと一字でも違えば通さない (大文字、符号)。
    let (content, sum) = lines[0].rsplit_once(" #").unwrap();
    for respelled in [sum.to_uppercase(), format!("+{}", &sum[1..])] {
        let first = format!("{content} #{respelled}");
        assert_ne!(first, lines[0]);
        let text = std::iter::once(first.as_str()).chain(lines[1..].iter().copied()).collect::<Vec<_>>().join("\n");
        assert!(matches!(replay(&text), Err(Stopped::Malformed { line: 1, .. })), "{first}");
    }
    // 知らない語は、何が読めなかったかを添えて止まる。
    short[2] = contents(&text).unwrap()[2].to_owned();
    short[3] = "> CreateProcess Bits48 4096".to_owned();
    assert_eq!(replay(&sealed(&short)).map(|replayed| replayed.steps), Err(Stopped::Malformed { line: 4, why: "知らないアドレスの幅: Bits48".to_owned() }));
}

/// 保存してある記録が、今の実装でも同じ返答で再生できる。kome の振る舞いか記録の形式を変えたら、
/// ここが止まった行を見て、意図した変更なら `KOME_UPDATE_GOLDEN=1 cargo test -p kome-trace` で作り直す。
#[test]
fn the_saved_recording_still_replays() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/basic.trace");
    let (text, recording) = record(&seeded(0, 400));
    if std::env::var_os("KOME_UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, &text).unwrap();
    }
    let saved = std::fs::read_to_string(&path).unwrap();
    let again = replayed(&saved);
    assert_eq!(&again.kernel, recording.kernel());
    assert_eq!(saved, text, "同じ操作の列の記録が、保存してあるものと違う");
}
