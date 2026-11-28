//! 入力の一行を、語の列として書く・読む。
//!
//! 書く側 ([`write_input`]) は [`Input`] の全ての項目を並べる (項目が増えると、ここがコンパイル
//! できなくなる)。読む側 ([`apply_input`]) は項目の名前で分ける。両方に全ての項目があることは、
//! 全ての項目を記録して再生する試験と、lint `trace-complete` が確かめる。

use std::fmt::Write as _;
use std::str::FromStr;

use kome::{
    AddrWidth, BackingOffset, Budget, ChannelSpec, Completion, CompletionKind, Deadline, Duration, End, Error, ExitCode,
    Framing, GuestAddr, GuestRange, HostError, Input, InstanceId, Instant, Interruptible, IoLinks, IoOutcome, Kernel, Limits,
    MapSource, MemFault, MemFaultKind, ObjectId, PageSize, Parcel, Period, Priority, Prot, Quota, Reply, RunPermit,
    SignalMode, SignalOp, SignalSpec, Start, Stop, WaitMode, WorkerId,
};

use crate::{Names, Tables, permit_key};

/// 16 進の文字列。空の列は `-`。
pub(crate) fn hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "-".to_owned();
    }
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        // String への書き込みは失敗しない。
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn unhex(word: &str) -> Result<Vec<u8>, String> {
    if word == "-" {
        return Ok(Vec::new());
    }
    let digits = word.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return Err(format!("16 進の桁数が奇数: {word}"));
    }
    digits
        .chunks(2)
        .map(|pair| std::str::from_utf8(pair).ok().and_then(|pair| u8::from_str_radix(pair, 16).ok()))
        .collect::<Option<Vec<u8>>>()
        .ok_or_else(|| format!("16 進ではない: {word}"))
}

/// 上限の行。
pub(crate) fn limits_line(limits: &Limits) -> String {
    let mut line = "limits".to_owned();
    limits.each(|name, value| {
        let _ = write!(line, " {name}={value}");
    });
    line
}

/// 上限の行を読む。全ての上限がちょうど一度ずつ無ければ `None`。
pub(crate) fn read_limits(line: &str) -> Option<Limits> {
    let mut words = line.split_ascii_whitespace();
    if words.next() != Some("limits") {
        return None;
    }
    let mut limits = Limits::DEFAULT;
    let mut seen = Vec::new();
    for word in words {
        let (name, value) = word.split_once('=')?;
        if seen.contains(&name) {
            return None;
        }
        seen.push(name);
        limits = limits.with_named(name, value.parse().ok()?)?;
    }
    let mut all = 0usize;
    limits.each(|_, _| all = all.saturating_add(1));
    (seen.len() == all).then_some(limits)
}

// ---- 書く ----

fn named<T: Copy + Ord>(prefix: char, names: &Names<T>, value: T) -> Option<String> {
    Some(format!("{prefix}{}", names.name(value)?))
}

fn object(tables: &Tables, object: ObjectId) -> Option<String> {
    named('o', &tables.objects, object)
}

const fn width_word(width: AddrWidth) -> &'static str {
    match width {
        AddrWidth::Bits32 => "Bits32",
        AddrWidth::Bits64 => "Bits64",
    }
}

const fn start_word(start: Start) -> &'static str {
    match start {
        Start::Ready => "Ready",
        Start::Suspended => "Suspended",
    }
}

const fn mode_word(mode: SignalMode) -> &'static str {
    match mode {
        SignalMode::Keep => "Keep",
        SignalMode::Consume => "Consume",
    }
}

const fn wait_word(mode: WaitMode) -> &'static str {
    match mode {
        WaitMode::Any => "Any",
        WaitMode::All => "All",
    }
}

const fn interruptible_word(interruptible: Interruptible) -> &'static str {
    match interruptible {
        Interruptible::Yes => "Yes",
        Interruptible::No => "No",
    }
}

const fn end_word(end: End) -> &'static str {
    match end {
        End::Send => "Send",
        End::Receive => "Receive",
    }
}

const fn fault_word(kind: MemFaultKind) -> &'static str {
    match kind {
        MemFaultKind::Unmapped => "Unmapped",
        MemFaultKind::Reserved => "Reserved",
        MemFaultKind::Denied => "Denied",
    }
}

fn prot_word(prot: Prot) -> String {
    let flag = |access, letter| if prot.allows(access) { letter } else { '-' };
    [flag(kome::Access::Read, 'r'), flag(kome::Access::Write, 'w'), flag(kome::Access::Execute, 'x')].iter().collect()
}

fn range_words(range: GuestRange) -> String {
    format!("{} {}", range.start().get(), range.last().get())
}

fn stop_words(stop: Stop) -> String {
    match stop {
        Stop::BudgetExhausted => "BudgetExhausted".to_owned(),
        Stop::Yielded => "Yielded".to_owned(),
        Stop::Interrupted => "Interrupted".to_owned(),
        Stop::Trap => "Trap".to_owned(),
        Stop::Fault(fault) => format!("Fault {} {}", fault.addr().get(), fault_word(fault.kind())),
        Stop::Exited(code) => format!("Exited {}", code.get()),
    }
}

fn outcome_words(outcome: IoOutcome) -> String {
    match outcome {
        IoOutcome::Done { transferred } => format!("Done {transferred}"),
        IoOutcome::Failed { error, transferred } => format!("Failed {} {transferred}", error.get()),
        IoOutcome::Cancelled { transferred } => format!("Cancelled {transferred}"),
    }
}

fn parcel_words(tables: &Tables, parcel: Option<Parcel>) -> Option<String> {
    Some(match parcel {
        None => "-".to_owned(),
        Some(Parcel::Object(held)) => format!("Object {}", object(tables, held)?),
        Some(Parcel::Backing(held)) => format!("Backing {}", named('b', &tables.backings, held)?),
    })
}

fn optional(tables: &Tables, held: Option<ObjectId>) -> Option<String> {
    held.map_or_else(|| Some("-".to_owned()), |held| object(tables, held))
}

/// 入力の行の内容 (先頭の印を除く)。この実行インスタンスが返していない ID か実行権を含めば `None`。
pub(crate) fn write_input(tables: &Tables, input: &Input<'_>) -> Option<String> {
    let process = |id| named('p', &tables.processes, id);
    let thread = |id| named('t', &tables.threads, id);
    let backing = |id| named('b', &tables.backings, id);
    let timer = |id| named('m', &tables.timers, id);
    let io = |id| named('i', &tables.ios, id);
    let channel = |id| named('c', &tables.channels, id);
    let object = |id| object(tables, id);
    Some(match input {
        Input::CreateProcess(width, page) => format!("CreateProcess {} {}", width_word(*width), page.bytes()),
        Input::ExitProcess(target, code) => format!("ExitProcess {} {}", process(*target)?, code.get()),
        Input::SetQuota(target, quota) => {
            let mut line = format!("SetQuota {}", process(*target)?);
            quota.each(|name, value| {
                let _ = write!(line, " {name}={value}");
            });
            line
        }
        Input::CreateThread(owner, start) => format!("CreateThread {} {}", process(*owner)?, start_word(*start)),
        Input::ContinueThread(target) => format!("ContinueThread {}", thread(*target)?),
        Input::SuspendThread(target) => format!("SuspendThread {}", thread(*target)?),
        Input::UnsuspendThread(target) => format!("UnsuspendThread {}", thread(*target)?),
        Input::TerminateThread(target, code) => format!("TerminateThread {} {}", thread(*target)?, code.get()),
        Input::Grant(worker) => format!("Grant {}", worker.get()),
        Input::GrantThread(worker, target) => format!("GrantThread {} {}", worker.get(), thread(*target)?),
        Input::PreemptThread(target) => format!("PreemptThread {}", thread(*target)?),
        Input::SetPriority(target, priority) => format!("SetPriority {} {}", thread(*target)?, priority.get()),
        Input::Relinquish(permit, stop, consumed) => {
            let lent = named('g', &tables.permits, permit_key(permit))?;
            format!("Relinquish {lent} {} {}", consumed.get(), stop_words(*stop))
        }
        Input::CreateObject(owner, spec) => {
            format!("CreateObject {} {} {} {}", process(*owner)?, mode_word(spec.mode()), spec.initial(), spec.max())
        }
        Input::RetainObject(holder, held) => format!("RetainObject {} {}", process(*holder)?, object(*held)?),
        Input::ReleaseObject(holder, held) => format!("ReleaseObject {} {}", process(*holder)?, object(*held)?),
        Input::Signal(target, op) => match op {
            SignalOp::Add(count) => format!("Signal {} Add {count}", object(*target)?),
            SignalOp::Set(count) => format!("Signal {} Set {count}", object(*target)?),
        },
        Input::Wait(waiter, targets, mode, deadline, interruptible) => {
            let mut line = format!("Wait {} {} {}", thread(*waiter)?, wait_word(*mode), interruptible_word(*interruptible));
            match deadline {
                Deadline::At(at) => {
                    let _ = write!(line, " At {}", at.nanos());
                }
                Deadline::Never => line.push_str(" Never"),
            }
            let _ = write!(line, " {}", targets.len());
            for target in *targets {
                let _ = write!(line, " {}", object(*target)?);
            }
            line
        }
        Input::InterruptWait(target) => format!("InterruptWait {}", thread(*target)?),
        Input::CreateTimer(owner, notify) => format!("CreateTimer {} {}", process(*owner)?, object(*notify)?),
        Input::ArmTimer(target, at, period) => {
            let period = period.map_or_else(|| "-".to_owned(), |period| period.get().nanos().to_string());
            format!("ArmTimer {} {} {period}", timer(*target)?, at.nanos())
        }
        Input::DisarmTimer(target) => format!("DisarmTimer {}", timer(*target)?),
        Input::DropTimer(target) => format!("DropTimer {}", timer(*target)?),
        Input::Advance(to) => format!("Advance {}", to.nanos()),
        Input::PrepareIo(owner, links) => {
            format!("PrepareIo {} {} {}", process(*owner)?, optional(tables, links.target())?, optional(tables, links.notify())?)
        }
        Input::IssueIo(target) => format!("IssueIo {}", io(*target)?),
        Input::CancelIo(target) => format!("CancelIo {}", io(*target)?),
        Input::Complete(completion) => match completion.kind() {
            CompletionKind::Io { io: target, outcome } => {
                format!("Complete {} Io {} {}", completion.instance().get(), io(target)?, outcome_words(outcome))
            }
        },
        Input::PublishIo(target) => format!("PublishIo {}", io(*target)?),
        Input::ReleaseIo(target) => format!("ReleaseIo {}", io(*target)?),
        Input::CreateBacking(owner, len) => format!("CreateBacking {} {len}", process(*owner)?),
        Input::RetainBacking(holder, held) => format!("RetainBacking {} {}", process(*holder)?, backing(*held)?),
        Input::ReleaseBacking(holder, held) => format!("ReleaseBacking {} {}", process(*holder)?, backing(*held)?),
        Input::Map(owner, range, source, prot) => {
            let source = match source {
                MapSource::Reserved => "Reserved".to_owned(),
                MapSource::Backing { backing: from, offset } => format!("Backing {} {}", backing(*from)?, offset.get()),
            };
            format!("Map {} {} {} {source}", process(*owner)?, range_words(*range), prot_word(*prot))
        }
        Input::Unmap(owner, range) => format!("Unmap {} {}", process(*owner)?, range_words(*range)),
        Input::Protect(owner, range, prot) => {
            format!("Protect {} {} {}", process(*owner)?, range_words(*range), prot_word(*prot))
        }
        Input::CreateChannel(owner, spec) => match spec.framing() {
            Framing::Messages { count, max_len } => {
                format!("CreateChannel {} Messages {count} {max_len}", process(*owner)?)
            }
            Framing::Stream { bytes } => format!("CreateChannel {} Stream {bytes}", process(*owner)?),
        },
        Input::RetainChannel(holder, held, end) => {
            format!("RetainChannel {} {} {}", process(*holder)?, channel(*held)?, end_word(*end))
        }
        Input::ReleaseChannel(holder, held, end) => {
            format!("ReleaseChannel {} {} {}", process(*holder)?, channel(*held)?, end_word(*end))
        }
        Input::Send(sender, through, bytes, parcel) => {
            format!("Send {} {} {} {}", process(*sender)?, channel(*through)?, hex(bytes), parcel_words(tables, *parcel)?)
        }
        Input::Receive(receiver, through, buffer) => {
            format!("Receive {} {} {}", process(*receiver)?, channel(*through)?, buffer.len())
        }
        Input::PollEffect => "PollEffect".to_owned(),
    })
}

// ---- 読む ----

/// 一行の語を順に読む。
struct Words<'a> {
    rest: std::str::SplitAsciiWhitespace<'a>,
}

impl<'a> Words<'a> {
    fn word(&mut self) -> Result<&'a str, String> {
        self.rest.next().ok_or_else(|| "語が足りない".to_owned())
    }

    fn number<T: FromStr>(&mut self) -> Result<T, String> {
        let word = self.word()?;
        word.parse().map_err(|_| format!("数ではない: {word}"))
    }

    fn named<T: Copy + Ord>(&mut self, prefix: char, names: &Names<T>) -> Result<T, String> {
        let word = self.word()?;
        let at = word.strip_prefix(prefix).and_then(|at| at.parse().ok());
        at.and_then(|at| names.get(at)).ok_or_else(|| format!("表に無い番号: {word}"))
    }

    fn end(mut self) -> Result<(), String> {
        self.rest.next().map_or(Ok(()), |extra| Err(format!("余分な語: {extra}")))
    }

    fn code(&mut self) -> Result<ExitCode, String> {
        Ok(ExitCode::new(self.number()?))
    }

    fn worker(&mut self) -> Result<WorkerId, String> {
        Ok(WorkerId::new(self.number()?))
    }

    fn instant(&mut self) -> Result<Instant, String> {
        Ok(Instant::from_nanos(self.number()?))
    }

    fn range(&mut self) -> Result<GuestRange, String> {
        let (start, last) = (GuestAddr::new(self.number()?), GuestAddr::new(self.number()?));
        GuestRange::spanning(start, last).ok_or_else(|| "範囲の両端が逆".to_owned())
    }

    fn prot(&mut self) -> Result<Prot, String> {
        let word = self.word()?;
        let flag = |at: usize, letter: u8| match word.as_bytes().get(at) {
            Some(found) if *found == letter => Ok(true),
            Some(b'-') => Ok(false),
            Some(_) | None => Err(format!("権限ではない: {word}")),
        };
        if word.len() != 3 {
            return Err(format!("権限ではない: {word}"));
        }
        Ok(Prot::new(flag(0, b'r')?, flag(1, b'w')?, flag(2, b'x')?))
    }

    fn optional(&mut self, tables: &Tables) -> Result<Option<kome::ObjectId>, String> {
        let mut ahead = self.rest.clone();
        if ahead.next() == Some("-") {
            self.rest = ahead;
            return Ok(None);
        }
        self.named('o', &tables.objects).map(Some)
    }

    fn stop(&mut self) -> Result<Stop, String> {
        Ok(match self.word()? {
            "BudgetExhausted" => Stop::BudgetExhausted,
            "Yielded" => Stop::Yielded,
            "Interrupted" => Stop::Interrupted,
            "Trap" => Stop::Trap,
            "Fault" => {
                let addr = GuestAddr::new(self.number()?);
                let kind = match self.word()? {
                    "Unmapped" => MemFaultKind::Unmapped,
                    "Reserved" => MemFaultKind::Reserved,
                    "Denied" => MemFaultKind::Denied,
                    other => return Err(unknown("メモリの失敗", other)),
                };
                Stop::Fault(MemFault::new(addr, kind))
            }
            "Exited" => Stop::Exited(self.code()?),
            other => return Err(unknown("止まった理由", other)),
        })
    }

    fn outcome(&mut self) -> Result<IoOutcome, String> {
        Ok(match self.word()? {
            "Done" => IoOutcome::Done { transferred: self.number()? },
            "Failed" => IoOutcome::Failed { error: HostError::new(self.number()?), transferred: self.number()? },
            "Cancelled" => IoOutcome::Cancelled { transferred: self.number()? },
            other => return Err(unknown("I/O の結果", other)),
        })
    }

    fn end_of_channel(&mut self) -> Result<End, String> {
        match self.word()? {
            "Send" => Ok(End::Send),
            "Receive" => Ok(End::Receive),
            other => Err(unknown("通信路の端", other)),
        }
    }
}

fn unknown(what: &str, word: &str) -> String {
    format!("知らない{what}: {word}")
}

/// 適用した結果と、受信したバイト列。
type Applied = (Result<Reply, Error>, Option<Vec<u8>>);

/// 入力の行の内容を読んで適用する。実行権は、貸した返答の番号の位置から取り出して使う。
pub(crate) fn apply_input(
    kernel: &mut Kernel,
    tables: &Tables,
    held: &mut [Option<RunPermit>],
    line: &str,
) -> Result<Applied, String> {
    let mut w = Words { rest: line.split_ascii_whitespace() };
    let mut received = None;
    let result = match w.word()? {
        "CreateProcess" => {
            let width = match w.word()? {
                "Bits32" => AddrWidth::Bits32,
                "Bits64" => AddrWidth::Bits64,
                other => return Err(unknown("アドレスの幅", other)),
            };
            let page = PageSize::new(w.number()?).ok_or("ページの大きさではない")?;
            w.end()?;
            kernel.apply(Input::CreateProcess(width, page))
        }
        "ExitProcess" => {
            let input = Input::ExitProcess(w.named('p', &tables.processes)?, w.code()?);
            w.end()?;
            kernel.apply(input)
        }
        "SetQuota" => {
            let process = w.named('p', &tables.processes)?;
            let (mut quota, mut seen) = (Quota::UNLIMITED, Vec::new());
            for word in w.rest {
                let (name, value) = word.split_once('=').ok_or_else(|| format!("割り当てではない: {word}"))?;
                let value = value.parse().map_err(|_| format!("数ではない: {word}"))?;
                quota = quota.with_named(name, value).ok_or_else(|| format!("割り当てではない: {word}"))?;
                if seen.contains(&name) {
                    return Err(format!("割り当てが二度ある: {word}"));
                }
                seen.push(name);
            }
            let mut all = 0usize;
            quota.each(|_, _| all = all.saturating_add(1));
            if seen.len() != all {
                return Err("割り当てが足りない".to_owned());
            }
            kernel.apply(Input::SetQuota(process, quota))
        }
        "CreateThread" => {
            let process = w.named('p', &tables.processes)?;
            let start = match w.word()? {
                "Ready" => Start::Ready,
                "Suspended" => Start::Suspended,
                other => return Err(unknown("始まり方", other)),
            };
            w.end()?;
            kernel.apply(Input::CreateThread(process, start))
        }
        "ContinueThread" => {
            let input = Input::ContinueThread(w.named('t', &tables.threads)?);
            w.end()?;
            kernel.apply(input)
        }
        "SuspendThread" => {
            let input = Input::SuspendThread(w.named('t', &tables.threads)?);
            w.end()?;
            kernel.apply(input)
        }
        "UnsuspendThread" => {
            let input = Input::UnsuspendThread(w.named('t', &tables.threads)?);
            w.end()?;
            kernel.apply(input)
        }
        "TerminateThread" => {
            let input = Input::TerminateThread(w.named('t', &tables.threads)?, w.code()?);
            w.end()?;
            kernel.apply(input)
        }
        "Grant" => {
            let input = Input::Grant(w.worker()?);
            w.end()?;
            kernel.apply(input)
        }
        "GrantThread" => {
            let input = Input::GrantThread(w.worker()?, w.named('t', &tables.threads)?);
            w.end()?;
            kernel.apply(input)
        }
        "PreemptThread" => {
            let input = Input::PreemptThread(w.named('t', &tables.threads)?);
            w.end()?;
            kernel.apply(input)
        }
        "SetPriority" => {
            let input = Input::SetPriority(w.named('t', &tables.threads)?, Priority::new(w.number()?));
            w.end()?;
            kernel.apply(input)
        }
        "Relinquish" => {
            let word = w.word()?;
            let at: Option<usize> = word.strip_prefix('g').and_then(|at| at.parse().ok());
            let permit = at.and_then(|at| held.get_mut(at)).and_then(Option::take);
            let permit = permit.ok_or_else(|| format!("手元に無い実行権: {word}"))?;
            let consumed = Budget::new(w.number()?);
            let stop = w.stop()?;
            w.end()?;
            kernel.apply(Input::Relinquish(permit, stop, consumed))
        }
        "CreateObject" => {
            let process = w.named('p', &tables.processes)?;
            let mode = match w.word()? {
                "Keep" => SignalMode::Keep,
                "Consume" => SignalMode::Consume,
                other => return Err(unknown("数の減らし方", other)),
            };
            let spec = SignalSpec::new(mode, w.number()?, w.number()?);
            w.end()?;
            kernel.apply(Input::CreateObject(process, spec))
        }
        "RetainObject" => {
            let input = Input::RetainObject(w.named('p', &tables.processes)?, w.named('o', &tables.objects)?);
            w.end()?;
            kernel.apply(input)
        }
        "ReleaseObject" => {
            let input = Input::ReleaseObject(w.named('p', &tables.processes)?, w.named('o', &tables.objects)?);
            w.end()?;
            kernel.apply(input)
        }
        "Signal" => {
            let object = w.named('o', &tables.objects)?;
            let op = match w.word()? {
                "Add" => SignalOp::Add(w.number()?),
                "Set" => SignalOp::Set(w.number()?),
                other => return Err(unknown("数の変更", other)),
            };
            w.end()?;
            kernel.apply(Input::Signal(object, op))
        }
        "Wait" => {
            let thread = w.named('t', &tables.threads)?;
            let mode = match w.word()? {
                "Any" => WaitMode::Any,
                "All" => WaitMode::All,
                other => return Err(unknown("待ち方", other)),
            };
            let interruptible = match w.word()? {
                "Yes" => Interruptible::Yes,
                "No" => Interruptible::No,
                other => return Err(unknown("割り込みの受け付け", other)),
            };
            let deadline = match w.word()? {
                "At" => Deadline::At(w.instant()?),
                "Never" => Deadline::Never,
                other => return Err(unknown("期限", other)),
            };
            let count: usize = w.number()?;
            let mut targets = Vec::new();
            for _ in 0..count {
                targets.push(w.named('o', &tables.objects)?);
            }
            w.end()?;
            kernel.apply(Input::Wait(thread, &targets, mode, deadline, interruptible))
        }
        "InterruptWait" => {
            let input = Input::InterruptWait(w.named('t', &tables.threads)?);
            w.end()?;
            kernel.apply(input)
        }
        "CreateTimer" => {
            let input = Input::CreateTimer(w.named('p', &tables.processes)?, w.named('o', &tables.objects)?);
            w.end()?;
            kernel.apply(input)
        }
        "ArmTimer" => {
            let (timer, at) = (w.named('m', &tables.timers)?, w.instant()?);
            let period = match w.word()? {
                "-" => None,
                nanos => {
                    let nanos = nanos.parse().map_err(|_| format!("数ではない: {nanos}"))?;
                    Some(Period::new(Duration::from_nanos(nanos)).ok_or("周期が 0")?)
                }
            };
            w.end()?;
            kernel.apply(Input::ArmTimer(timer, at, period))
        }
        "DisarmTimer" => {
            let input = Input::DisarmTimer(w.named('m', &tables.timers)?);
            w.end()?;
            kernel.apply(input)
        }
        "DropTimer" => {
            let input = Input::DropTimer(w.named('m', &tables.timers)?);
            w.end()?;
            kernel.apply(input)
        }
        "Advance" => {
            let input = Input::Advance(w.instant()?);
            w.end()?;
            kernel.apply(input)
        }
        "PrepareIo" => {
            let process = w.named('p', &tables.processes)?;
            let mut links = IoLinks::NONE;
            if let Some(target) = w.optional(tables)? {
                links = links.with_target(target);
            }
            if let Some(notify) = w.optional(tables)? {
                links = links.with_notify(notify);
            }
            w.end()?;
            kernel.apply(Input::PrepareIo(process, links))
        }
        "IssueIo" => {
            let input = Input::IssueIo(w.named('i', &tables.ios)?);
            w.end()?;
            kernel.apply(input)
        }
        "CancelIo" => {
            let input = Input::CancelIo(w.named('i', &tables.ios)?);
            w.end()?;
            kernel.apply(input)
        }
        "Complete" => {
            let instance = InstanceId::new(w.number()?);
            let kind = match w.word()? {
                "Io" => CompletionKind::Io { io: w.named('i', &tables.ios)?, outcome: w.outcome()? },
                other => return Err(unknown("応答の内容", other)),
            };
            w.end()?;
            kernel.apply(Input::Complete(Completion::new(instance, kind)))
        }
        "PublishIo" => {
            let input = Input::PublishIo(w.named('i', &tables.ios)?);
            w.end()?;
            kernel.apply(input)
        }
        "ReleaseIo" => {
            let input = Input::ReleaseIo(w.named('i', &tables.ios)?);
            w.end()?;
            kernel.apply(input)
        }
        "CreateBacking" => {
            let input = Input::CreateBacking(w.named('p', &tables.processes)?, w.number()?);
            w.end()?;
            kernel.apply(input)
        }
        "RetainBacking" => {
            let input = Input::RetainBacking(w.named('p', &tables.processes)?, w.named('b', &tables.backings)?);
            w.end()?;
            kernel.apply(input)
        }
        "ReleaseBacking" => {
            let input = Input::ReleaseBacking(w.named('p', &tables.processes)?, w.named('b', &tables.backings)?);
            w.end()?;
            kernel.apply(input)
        }
        "Map" => {
            let (process, range, prot) = (w.named('p', &tables.processes)?, w.range()?, w.prot()?);
            let source = match w.word()? {
                "Reserved" => MapSource::Reserved,
                "Backing" => MapSource::Backing {
                    backing: w.named('b', &tables.backings)?,
                    offset: BackingOffset::new(w.number()?),
                },
                other => return Err(unknown("マッピングの元", other)),
            };
            w.end()?;
            kernel.apply(Input::Map(process, range, source, prot))
        }
        "Unmap" => {
            let input = Input::Unmap(w.named('p', &tables.processes)?, w.range()?);
            w.end()?;
            kernel.apply(input)
        }
        "Protect" => {
            let input = Input::Protect(w.named('p', &tables.processes)?, w.range()?, w.prot()?);
            w.end()?;
            kernel.apply(input)
        }
        "CreateChannel" => {
            let process = w.named('p', &tables.processes)?;
            let spec = match w.word()? {
                "Messages" => ChannelSpec::messages(w.number()?, w.number()?),
                "Stream" => ChannelSpec::stream(w.number()?),
                other => return Err(unknown("通信路の形", other)),
            };
            w.end()?;
            kernel.apply(Input::CreateChannel(process, spec))
        }
        "RetainChannel" => {
            let input =
                Input::RetainChannel(w.named('p', &tables.processes)?, w.named('c', &tables.channels)?, w.end_of_channel()?);
            w.end()?;
            kernel.apply(input)
        }
        "ReleaseChannel" => {
            let input =
                Input::ReleaseChannel(w.named('p', &tables.processes)?, w.named('c', &tables.channels)?, w.end_of_channel()?);
            w.end()?;
            kernel.apply(input)
        }
        "Send" => {
            let (process, channel) = (w.named('p', &tables.processes)?, w.named('c', &tables.channels)?);
            let bytes = unhex(w.word()?)?;
            let parcel = match w.word()? {
                "-" => None,
                "Object" => Some(Parcel::Object(w.named('o', &tables.objects)?)),
                "Backing" => Some(Parcel::Backing(w.named('b', &tables.backings)?)),
                other => return Err(unknown("運ぶ資源", other)),
            };
            w.end()?;
            kernel.apply(Input::Send(process, channel, &bytes, parcel))
        }
        "Receive" => {
            let (process, channel) = (w.named('p', &tables.processes)?, w.named('c', &tables.channels)?);
            let len: usize = w.number()?;
            w.end()?;
            let mut buffer = Vec::new();
            buffer.try_reserve_exact(len).map_err(|_| format!("受信のバッファを確保できない: {len}"))?;
            buffer.resize(len, 0u8);
            let result = kernel.apply(Input::Receive(process, channel, &mut buffer));
            received = crate::received(&result, &buffer).map(<[u8]>::to_vec);
            result
        }
        "PollEffect" => {
            w.end()?;
            kernel.apply(Input::PollEffect)
        }
        other => return Err(unknown("操作", other)),
    };
    Ok((result, received))
}
