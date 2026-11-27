//! 制御可能なホスト: メモリの内容と、ファイル読み込みの完了を試験が決める。
//!
//! kome は内容を持たない。ここが実体のバイト列とファイルを持ち、`Effect` を受けて
//! 完了を予定する。完了が届く時刻、取消と完了のどちらが勝つか、一度に返す量は
//! 試験が決める。実時間は待たない。
//!
//! I/O のバッファは、ホストが所有して完了と一緒に返すコピー経路である。

use std::collections::BTreeMap;

use kome::{BackingId, Duration, Effect, HostError, Instant, IoId, IoOutcome};

/// 取消の要求が届いたとき、まだ完了していない読み込みをどうするか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelPolicy {
    /// 取り消す。完了は「取消」になる。
    CancelWins,
    /// 取消は間に合わない。予定どおり完了する。
    CompletionWins,
}

/// ファイルの識別。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileId(pub u32);

#[derive(Clone, Copy, Debug)]
struct Read {
    file: FileId,
    offset: u64,
    len: u64,
}

/// 届いた完了: どの要求が、どの結果で、どのバイト列を返したか。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arrived {
    /// 終端した要求。
    pub io: IoId,
    /// 結果。
    pub outcome: IoOutcome,
    /// 読んだ内容 (ホストが所有していたバッファ)。
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
struct Scheduled {
    at: Instant,
    arrived: Arrived,
}

/// ホストの振る舞い。試験が決める。
#[derive(Clone, Copy, Debug)]
pub struct Behavior {
    /// 読み込みが完了するまでの仮想時間。
    pub latency: Duration,
    /// 一度の読み込みが返す最大のバイト数。
    pub chunk: u64,
    /// 取消の扱い。
    pub cancel: CancelPolicy,
    /// 読み込みを失敗させるなら、その番号。
    pub fail: Option<HostError>,
}

impl Default for Behavior {
    fn default() -> Behavior {
        Behavior { latency: Duration::from_nanos(0), chunk: u64::MAX, cancel: CancelPolicy::CancelWins, fail: None }
    }
}

/// 制御可能なホスト。
#[derive(Clone, Debug)]
pub struct FakeHost {
    memory: BTreeMap<BackingId, Vec<u8>>,
    files: BTreeMap<FileId, Vec<u8>>,
    reads: BTreeMap<IoId, Read>,
    scheduled: Vec<Scheduled>,
    behavior: Behavior,
}

impl FakeHost {
    /// 決めた振る舞いのホストを作る。
    #[must_use]
    pub fn new(behavior: Behavior) -> FakeHost {
        FakeHost { memory: BTreeMap::new(), files: BTreeMap::new(), reads: BTreeMap::new(), scheduled: Vec::new(), behavior }
    }

    /// 実体の内容を確保する。
    pub fn alloc(&mut self, backing: BackingId, len: u64) -> Result<(), String> {
        let len = usize::try_from(len).map_err(|error| error.to_string())?;
        match self.memory.insert(backing, vec![0; len]) {
            None => Ok(()),
            Some(_) => Err(format!("{backing:?} を二度確保した")),
        }
    }

    /// 実体の内容。
    #[must_use]
    pub fn bytes(&self, backing: BackingId) -> Option<&[u8]> {
        self.memory.get(&backing).map(Vec::as_slice)
    }

    /// 実体の `offset` へ書く。範囲の外なら失敗する。
    pub fn write(&mut self, backing: BackingId, offset: u64, data: &[u8]) -> Result<(), String> {
        let start = usize::try_from(offset).map_err(|error| error.to_string())?;
        let end = start.checked_add(data.len()).ok_or("範囲")?;
        let memory = self.memory.get_mut(&backing).ok_or(format!("{backing:?} は確保されていない"))?;
        memory.get_mut(start..end).ok_or("実体の外への書き込み")?.copy_from_slice(data);
        Ok(())
    }

    /// 内容を確保している実体の数。
    #[must_use]
    pub fn live_backings(&self) -> usize {
        self.memory.len()
    }

    /// ファイルを置く。
    pub fn add_file(&mut self, file: FileId, contents: &[u8]) {
        self.files.insert(file, contents.to_vec());
    }

    /// 要求の内容を登録する。実行統括が、要求を発行する前に呼ぶ。
    pub fn expect_read(&mut self, io: IoId, file: FileId, offset: u64, len: u64) {
        self.reads.insert(io, Read { file, offset, len });
    }

    /// kome の `Effect` を処理する。ホストに関係の無いものは何もしない。
    pub fn on_effect(&mut self, effect: Effect, now: Instant) -> Result<(), String> {
        match effect {
            Effect::IoSubmit { io } => {
                let read = self.reads.remove(&io).ok_or(format!("{io:?} の内容が登録されていない"))?;
                let at = now.checked_add(self.behavior.latency).ok_or("時刻")?;
                let arrived = match self.behavior.fail {
                    Some(error) => Arrived { io, outcome: IoOutcome::Failed { error, transferred: 0 }, data: Vec::new() },
                    None => {
                        let contents = self.files.get(&read.file).ok_or(format!("{:?} が無い", read.file))?;
                        let start = usize::try_from(read.offset).unwrap_or(usize::MAX).min(contents.len());
                        let want = usize::try_from(read.len.min(self.behavior.chunk)).unwrap_or(usize::MAX);
                        let end = start.saturating_add(want).min(contents.len());
                        let data = contents.get(start..end).ok_or("範囲")?.to_vec();
                        let transferred = u64::try_from(data.len()).map_err(|error| error.to_string())?;
                        Arrived { io, outcome: IoOutcome::Done { transferred }, data }
                    }
                };
                self.scheduled.push(Scheduled { at, arrived });
            }
            Effect::IoCancel { io } => match self.behavior.cancel {
                CancelPolicy::CompletionWins => {}
                CancelPolicy::CancelWins => {
                    for scheduled in self.scheduled.iter_mut().filter(|scheduled| scheduled.arrived.io == io) {
                        scheduled.at = now;
                        scheduled.arrived.outcome = IoOutcome::Cancelled { transferred: 0 };
                        scheduled.arrived.data.clear();
                    }
                }
            },
            Effect::BackingReclaimed { backing } => {
                self.memory.remove(&backing).ok_or(format!("確保していない {backing:?} の解放"))?;
            }
            Effect::Interrupt { .. }
            | Effect::WaitDone { .. }
            | Effect::ThreadExited { .. }
            | Effect::ProcessExited { .. }
            | Effect::ProcessReclaimed { .. }
            | Effect::ObjectReclaimed { .. } => {}
        }
        Ok(())
    }

    /// 最も早い完了の時刻。
    #[must_use]
    pub fn next_completion(&self) -> Option<Instant> {
        self.scheduled.iter().map(|scheduled| scheduled.at).min()
    }

    /// `now` までに届く完了を、予定した順に取り出す。
    pub fn arrivals(&mut self, now: Instant) -> Vec<Arrived> {
        let (due, later): (Vec<Scheduled>, Vec<Scheduled>) =
            std::mem::take(&mut self.scheduled).into_iter().partition(|scheduled| scheduled.at <= now);
        self.scheduled = later;
        due.into_iter().map(|scheduled| scheduled.arrived).collect()
    }
}
