//! kome に入った入力列の記録と再生。
//!
//! [`Recording`] は、空の [`Kernel`] から始めて、適用した [`Input`] とその返答を一行ずつ文章に残す。
//! [`replay`] は、その文章を先頭から適用し直し、返答が記録と違った最初の行で止まる。読み飛ばしも
//! 並べ替えもしない。`Effect` は [`Input::PollEffect`] の返答として記録に入る。全ての返答の行には、
//! 操作の後の状態のあらまし (取り出していない `Effect` の数、貸せるスレッドの数、貸している実行権の数、
//! 時刻、生きている資源の種類ごとの数) も入るので、返答が同じでも状態がずれれば、その行で止まる。
//!
//! # 形式
//!
//! 一行が一つの項目で、行の終わりに、先頭からその行までの内容を繋げた照合値が付く。行を抜く、
//! 入れ替える、書き換えると、照合値が合わなくなる。
//!
//! ```text
//! kome-trace 1 #…            形式の版
//! instance 7 #…              実行インスタンスの識別
//! limits threads=256 … #…    上限 (全て)
//! > CreateProcess Bits64 4096 #…          入力
//! < effects=0 ready=0 running=0 now=0 alive=1/0/1/0/0/0/0 Ok(Process(…, …)) #…
//!                                         操作の後の状態のあらましと、返答
//! ```
//!
//! 入力の中の ID は、それを返した返答の順に付けた番号 (`p0`、`t3`、`o12` …) で書く。実行権は、
//! 貸した返答の順の番号 (`g5`) で書く。再生する側は、自分の返答から同じ番号の表を作る。だから
//! 記録から ID や実行権を作り出すことは無い。
//!
//! この実行インスタンスが返したことの無い ID や実行権を含む入力は、再生できない。その行は `?` で
//! 始まり、[`replay`] はそこで [`Stopped::NotReplayable`] を返す。
//!
//! 入力の行は、適用する前に書く。適用の途中で落ちた記録は、返答の無い入力の行で終わり、
//! [`replay`] はその入力を適用した結果を [`Replayed::unanswered`] で返す。

mod words;

use std::collections::BTreeMap;
use std::fmt::Write as _;

use kome::{
    BackingId, ChannelId, Error, Input, InstanceId, IoId, Kernel, Limits, ObjectId, ProcessId, Received, Reply, RunGen,
    RunPermit, ThreadId, TimerId, WorkerId,
};

/// 形式の版を表す最初の行。
const MAGIC: &str = "kome-trace 1";

/// 行の内容を、前の行までの照合値へ繋げる (FNV-1a)。
fn chain(previous: u64, line: &str) -> u64 {
    let mut sum = previous ^ 0xcbf2_9ce4_8422_2325;
    for byte in line.bytes().chain(*b"\n") {
        sum = (sum ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    sum
}

/// 照合値を付けた行の列を作る。記録を作る側と、試験が記録を作り直すときに使う。
#[derive(Debug, Default)]
pub struct Sealer {
    text: String,
    sum: u64,
}

impl Sealer {
    /// 一行を、照合値を付けて足す。
    pub fn line(&mut self, content: &str) {
        self.sum = chain(self.sum, content);
        // String への書き込みは失敗しない。
        let _ = writeln!(self.text, "{content} #{:016x}", self.sum);
    }

    /// まだ取り出していない行を取り出す。照合値の続きは保つ。
    pub fn take(&mut self) -> String {
        std::mem::take(&mut self.text)
    }
}

/// 照合値を外した行の内容を取り出す。記録を書き換える試験と、記録を読む道具が使う。
/// 照合値の無い行があれば `None`。
#[must_use]
pub fn contents(text: &str) -> Option<Vec<&str>> {
    text.lines().map(|line| line.rsplit_once(" #").map(|(content, _)| content)).collect()
}

/// 返答の順に付けた番号の表。
#[derive(Debug)]
struct Names<T> {
    order: Vec<T>,
    index: BTreeMap<T, usize>,
}

impl<T: Copy + Ord> Names<T> {
    const fn new() -> Names<T> {
        Names { order: Vec::new(), index: BTreeMap::new() }
    }

    fn note(&mut self, value: T) -> usize {
        let next = self.order.len();
        let at = *self.index.entry(value).or_insert(next);
        if at == next {
            self.order.push(value);
        }
        at
    }

    fn name(&self, value: T) -> Option<usize> {
        self.index.get(&value).copied()
    }

    fn get(&self, at: usize) -> Option<T> {
        self.order.get(at).copied()
    }
}

/// 実行権を見分ける組。一つのスレッドの実行の世代は、貸すたびに進む。
type PermitKey = (ThreadId, WorkerId, RunGen);

fn permit_key(permit: &RunPermit) -> PermitKey {
    (permit.thread(), permit.worker(), permit.run())
}

/// 返答に現れた ID と実行権の番号。記録する側と再生する側が、同じ返答から同じ表を作る。
#[derive(Debug)]
struct Tables {
    processes: Names<ProcessId>,
    threads: Names<ThreadId>,
    objects: Names<ObjectId>,
    backings: Names<BackingId>,
    timers: Names<TimerId>,
    ios: Names<IoId>,
    channels: Names<ChannelId>,
    permits: Names<PermitKey>,
}

impl Tables {
    const fn new() -> Tables {
        Tables {
            processes: Names::new(),
            threads: Names::new(),
            objects: Names::new(),
            backings: Names::new(),
            timers: Names::new(),
            ios: Names::new(),
            channels: Names::new(),
            permits: Names::new(),
        }
    }

    /// 返答が新しく返した ID と実行権に番号を付ける。
    fn learn(&mut self, reply: &Reply) {
        match reply {
            Reply::Process(process, exit) => {
                self.processes.note(*process);
                self.objects.note(*exit);
            }
            Reply::Thread(thread, exit) => {
                self.threads.note(*thread);
                self.objects.note(*exit);
            }
            Reply::Object(object) => {
                self.objects.note(*object);
            }
            Reply::Backing(backing) => {
                self.backings.note(*backing);
            }
            Reply::Timer(timer) => {
                self.timers.note(*timer);
            }
            Reply::Io(io) => {
                self.ios.note(*io);
            }
            Reply::Channel(made) => {
                self.channels.note(made.channel());
                self.objects.note(made.readable());
                self.objects.note(made.writable());
            }
            Reply::Permit(Some(permit)) | Reply::Returned(permit, _) => {
                self.permits.note(permit_key(permit));
            }
            Reply::Permit(None)
            | Reply::Done
            | Reply::Quota(_)
            | Reply::Priority(_)
            | Reply::Fate(_)
            | Reply::Effect(_)
            | Reply::Count(_)
            | Reply::Advanced(_)
            | Reply::WasArmed(_)
            | Reply::Wait(_)
            | Reply::Cancel(_)
            | Reply::Settled(_)
            | Reply::Outcome(_)
            | Reply::Generation(_)
            | Reply::Sent(_)
            | Reply::Received(_) => {}
        }
    }
}

/// 返答の行の内容。操作の後の状態のあらまし (取り出していない `Effect` の数、貸せるスレッドの数、
/// 貸している実行権の数、時刻、生きている資源の数)、返答、受信したバイト列。
fn answer(kernel: &Kernel, result: &Result<Reply, Error>, received: Option<&[u8]>) -> String {
    let census = kernel.census();
    let mut line = format!(
        "< effects={} ready={} running={} now={} alive={}/{}/{}/{}/{}/{}/{} {result:?}",
        kernel.pending_effects(),
        kernel.ready_count(),
        kernel.running_count(),
        kernel.now().nanos(),
        census.processes(),
        census.threads(),
        census.objects(),
        census.backings(),
        census.timers(),
        census.ios(),
        census.channels(),
    );
    if let Some(bytes) = received {
        line.push_str(" | ");
        line.push_str(&words::hex(bytes));
    }
    line
}

/// 受信が書いたバイト列。
fn received<'a>(result: &Result<Reply, Error>, buffer: &'a [u8]) -> Option<&'a [u8]> {
    match result {
        Ok(Reply::Received(Received::Data { bytes, .. })) => buffer.get(..usize::try_from(*bytes).ok()?),
        Ok(_) | Err(_) => None,
    }
}

/// 記録しながら動かす [`Kernel`]。
///
/// 空の状態から始める。記録に残るのは [`Recording::apply`] を通した操作だけなので、状態を変える
/// 入口はこれだけにしてある。
#[derive(Debug)]
pub struct Recording {
    kernel: Kernel,
    tables: Tables,
    out: Sealer,
}

impl Recording {
    /// 空の状態を作り、記録を始める。
    pub fn new(instance: InstanceId, limits: Limits) -> Result<Recording, Error> {
        let kernel = Kernel::new(instance, limits)?;
        let mut out = Sealer::default();
        out.line(MAGIC);
        out.line(&format!("instance {}", instance.get()));
        out.line(&words::limits_line(&limits));
        Ok(Recording { kernel, tables: Tables::new(), out })
    }

    /// 状態を読む。
    #[must_use]
    pub const fn kernel(&self) -> &Kernel {
        &self.kernel
    }

    /// 一つの操作を記録して適用する。返すものは [`Kernel::apply`] と同じである。
    pub fn apply(&mut self, input: Input<'_>) -> Result<Reply, Error> {
        match words::write_input(&self.tables, &input) {
            Some(line) => self.out.line(&format!("> {line}")),
            None => self.out.line(&format!("? {input:?}")),
        }
        let (result, data) = if let Input::Receive(process, channel, buffer) = input {
            let result = self.kernel.apply(Input::Receive(process, channel, &mut *buffer));
            let data = received(&result, buffer).map(<[u8]>::to_vec);
            (result, data)
        } else {
            (self.kernel.apply(input), None)
        };
        if let Ok(reply) = &result {
            self.tables.learn(reply);
        }
        self.out.line(&answer(&self.kernel, &result, data.as_deref()));
        result
    }

    /// まだ取り出していない記録の行を取り出す。取り出した文章を順に繋げたものが、記録の全体である。
    pub fn take_text(&mut self) -> String {
        self.out.take()
    }
}

/// 再生が最後まで進んだ結果。
#[derive(Debug)]
pub struct Replayed {
    /// 記録の入力を全て適用した後の状態。
    pub kernel: Kernel,
    /// 適用した入力の数。
    pub steps: u64,
    /// 記録が返答の無い入力で終わっていたとき、その入力を適用した結果の行。
    pub unanswered: Option<String>,
}

/// 再生が途中で止まった理由。行の番号は 1 から数える。
#[derive(Debug, PartialEq, Eq)]
pub enum Stopped {
    /// 記録として読めない (形式の違い、照合値の不一致、表に無い番号)。
    Malformed {
        /// 読めなかった行。
        line: usize,
        /// 理由。
        why: String,
    },
    /// 返答か、操作の後の状態のあらましか、受信したバイト列が記録と違う。
    Mismatch {
        /// 記録の返答の行。
        line: usize,
        /// 記録にある内容。
        recorded: String,
        /// 再生で得た内容。
        replayed: String,
    },
    /// この実行インスタンスが返していない ID か実行権を含む入力で、再生できない。
    NotReplayable {
        /// その入力の行。
        line: usize,
        /// 記録に残っている入力の表示。
        input: String,
    },
}

/// 記録を先頭から適用し直す。
///
/// 返答が記録と違った最初の行で止まる。記録に無い入力を補うことも、合わない行を読み飛ばすことも無い。
pub fn replay(text: &str) -> Result<Replayed, Stopped> {
    let mut lines = Lines { rest: text.lines(), at: 0, sum: 0 };
    let malformed = |line: usize, why: &str| Stopped::Malformed { line, why: why.to_owned() };
    if lines.next()? != Some(MAGIC) {
        return Err(malformed(1, "形式の版の行が無い"));
    }
    let instance = lines.next()?.and_then(|line| line.strip_prefix("instance ")).and_then(|word| word.parse().ok());
    let instance = InstanceId::new(instance.ok_or_else(|| malformed(2, "実行インスタンスの識別の行が無い"))?);
    let limits = lines.next()?.and_then(words::read_limits).ok_or_else(|| malformed(3, "上限の行が読めない"))?;
    let mut kernel = Kernel::new(instance, limits).map_err(|error| malformed(3, &format!("上限で状態を作れない: {error:?}")))?;
    let mut tables = Tables::new();
    let mut held: Vec<Option<RunPermit>> = Vec::new();
    let mut steps = 0u64;
    while let Some(line) = lines.next()? {
        let at = lines.at;
        if let Some(input) = line.strip_prefix("? ") {
            return Err(Stopped::NotReplayable { line: at, input: input.to_owned() });
        }
        let input = line.strip_prefix("> ").ok_or_else(|| malformed(at, "入力の行ではない"))?;
        let (result, data) = words::apply_input(&mut kernel, &tables, &mut held, input)
            .map_err(|why| Stopped::Malformed { line: at, why })?;
        steps = steps.saturating_add(1);
        let replayed = answer(&kernel, &result, data.as_deref());
        if let Ok(reply) = result {
            tables.learn(&reply);
            keep(&tables, &mut held, reply);
        }
        let Some(recorded) = lines.next()? else {
            return Ok(Replayed { kernel, steps, unanswered: Some(replayed) });
        };
        if recorded != replayed {
            return Err(Stopped::Mismatch { line: lines.at, recorded: recorded.to_owned(), replayed });
        }
    }
    Ok(Replayed { kernel, steps, unanswered: None })
}

/// 返答が貸した実行権 (と、受理されずに戻ってきた実行権) を、番号の位置に持っておく。
fn keep(tables: &Tables, held: &mut Vec<Option<RunPermit>>, reply: Reply) {
    let permit = match reply {
        Reply::Permit(Some(permit)) | Reply::Returned(permit, _) => permit,
        Reply::Permit(None)
        | Reply::Done
        | Reply::Process(..)
        | Reply::Thread(..)
        | Reply::Object(_)
        | Reply::Backing(_)
        | Reply::Timer(_)
        | Reply::Io(_)
        | Reply::Quota(_)
        | Reply::Priority(_)
        | Reply::Fate(_)
        | Reply::Effect(_)
        | Reply::Count(_)
        | Reply::Advanced(_)
        | Reply::WasArmed(_)
        | Reply::Wait(_)
        | Reply::Cancel(_)
        | Reply::Settled(_)
        | Reply::Outcome(_)
        | Reply::Generation(_)
        | Reply::Channel(_)
        | Reply::Sent(_)
        | Reply::Received(_) => return,
    };
    let Some(at) = tables.permits.name(permit_key(&permit)) else { return };
    if held.len() <= at {
        held.resize_with(at.saturating_add(1), || None);
    }
    if let Some(slot) = held.get_mut(at) {
        *slot = Some(permit);
    }
}

/// 照合値を確かめながら行を読む。
struct Lines<'a> {
    rest: std::str::Lines<'a>,
    /// 最後に読んだ行の番号 (1 から)。
    at: usize,
    sum: u64,
}

impl<'a> Lines<'a> {
    fn next(&mut self) -> Result<Option<&'a str>, Stopped> {
        let Some(line) = self.rest.next() else { return Ok(None) };
        self.at = self.at.saturating_add(1);
        let bad = |why: &str| Stopped::Malformed { line: self.at, why: why.to_owned() };
        let (content, sum) = line.rsplit_once(" #").ok_or_else(|| bad("照合値が無い"))?;
        let expected = chain(self.sum, content);
        // 書いたときと同じ綴りだけを通す。数として読むと、大文字や符号を付けた書き換えが通ってしまう。
        if sum != format!("{expected:016x}") {
            return Err(bad("照合値が合わない (行が抜けたか、入れ替わったか、書き換わった)"));
        }
        self.sum = expected;
        Ok(Some(content))
    }
}
