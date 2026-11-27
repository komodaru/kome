//! 外部処理の要求 ([`Effect`]) と、外部から返る結果 ([`Completion`])。
//!
//! kome の状態遷移は外部を読まない。必要な外部処理は `Effect` としてキューへ出し、
//! ホスト実装と実行統括が処理して、結果を `Completion` として戻す。
//!
//! 受理した操作が将来必要とする `Effect` の席は、受理する時点で予約する (`promise`)。
//! 予約できなければ受理前に `Exhausted(Effects)` を返す。予約済みの `Effect` を出す
//! 操作 (`emit`) は確保せず、失敗しない (台帳 FX-1)。

use crate::error::{Error, Invariant, Resource};
use crate::id::{BackingId, IoId, ObjectId, ProcessId, ThreadId};
use crate::io::IoOutcome;
use crate::kernel::InstanceId;
use crate::process::ExitCode;
use crate::sched::{RunGen, WorkerId};
use crate::store::{Queue, TryClone, plain_try_clone};
use crate::wait::WaitOutcome;

/// kome が外へ求める処理、および外が知るべき確定事項。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// 実行中の worker を、状態を回収できる地点で止めて実行権を返させる。
    Interrupt {
        /// 実行権を持つ worker。
        worker: WorkerId,
        /// 実行中のスレッド。
        thread: ThreadId,
        /// 対象の実行。これより新しい実行に対しては無視する。
        run: RunGen,
    },
    /// 待機の結果が確定した。スレッドは「サービス中」に戻っている。
    WaitDone {
        /// 待機していたスレッド。
        thread: ThreadId,
        /// 確定した結果。
        outcome: WaitOutcome,
    },
    /// スレッドが終了した。ID はもう何も指さない。
    ThreadExited {
        /// 終了したスレッド。
        thread: ThreadId,
        /// 所属していたプロセス。
        process: ProcessId,
        /// 終了コード。
        code: ExitCode,
    },
    /// プロセスの全スレッドが終了し、参照とアドレス空間を手放した。
    ProcessExited {
        /// 終了したプロセス。
        process: ProcessId,
        /// 終了コード。
        code: ExitCode,
    },
    /// プロセスの未完了 I/O もすべて終端した。ID はもう何も指さない。
    ProcessReclaimed {
        /// 回収したプロセス。
        process: ProcessId,
    },
    /// 待機可能な資源を、参照も使用も無くなったので破棄した。
    ObjectReclaimed {
        /// 破棄した資源。
        object: ObjectId,
    },
    /// 実体を、参照もマッピングも旧世代の実行も無くなったので解放してよい。
    BackingReclaimed {
        /// 解放してよい実体。
        backing: BackingId,
    },
    /// I/O を開始する。内容は実行統括が `io` に対応づけて持つ。
    IoSubmit {
        /// 開始する要求。
        io: IoId,
    },
    /// I/O の取消を要求する。取消の完了ではない。終端結果は `Completion` で確定する。
    IoCancel {
        /// 取消を要求する要求。
        io: IoId,
    },
}

/// 外部処理の結果。どの実行インスタンスの要求への応答かを必ず持つ。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Completion {
    instance: InstanceId,
    kind: CompletionKind,
}

impl Completion {
    /// `instance` の `Kernel` が出した要求への応答を作る。
    #[must_use]
    pub const fn new(instance: InstanceId, kind: CompletionKind) -> Completion {
        Completion { instance, kind }
    }

    /// 応答先の実行インスタンス。
    #[must_use]
    pub const fn instance(&self) -> InstanceId {
        self.instance
    }

    /// 応答の内容。
    #[must_use]
    pub const fn kind(&self) -> CompletionKind {
        self.kind
    }
}

/// 外部処理の結果の内容。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    /// I/O がホスト側で終端した。
    Io {
        /// 終端した要求。
        io: IoId,
        /// ホストが報告する結果。
        outcome: IoOutcome,
    },
}

plain_try_clone!(Effect);

impl TryClone for Effects {
    fn try_clone(&self) -> Result<Effects, Error> {
        let Effects { queue, seats } = self;
        Ok(Effects { queue: queue.try_clone()?, seats: *seats })
    }
}

/// 席の勘定。キューの中身を持たず、整数だけで数える。
///
/// キューに入っている数は呼び出し側が渡す。確保を伴わないので、Kani は上限・予約数・キューの数の
/// 全ての値について、予約が上限を超えないことを確かめられる (証明 `seats_never_promise_beyond_the_limit`)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Seats {
    /// まだ出していないが、出すことを約束した `Effect` の数。
    promised: u32,
    limit: u32,
}

impl Seats {
    pub(crate) const fn new(limit: u32) -> Seats {
        Seats { promised: 0, limit }
    }

    /// キューに `queued` 個あるときに使っている席の数。
    fn used(&self, queued: u64) -> u64 {
        queued.saturating_add(u64::from(self.promised))
    }

    /// キューに `queued` 個あるとき、あと `count` 個を予約できるか。
    pub(crate) fn room_for(&self, queued: u64, count: u32) -> Result<(), Error> {
        if self.used(queued).saturating_add(u64::from(count)) > u64::from(self.limit) {
            return Err(Error::Exhausted(Resource::Effects));
        }
        Ok(())
    }

    /// `count` 個の席を予約する。
    pub(crate) fn promise(&mut self, queued: u64, count: u32) -> Result<(), Error> {
        self.room_for(queued, count)?;
        self.promised = self.promised.checked_add(count).ok_or(Error::Broken(Invariant::EffectsKept))?;
        Ok(())
    }

    /// 使わなかった予約を返す。予約した `Effect` を出すときも、その一席をここで返す。
    pub(crate) fn forgo(&mut self, count: u32) -> Result<(), Error> {
        self.promised = self.promised.checked_sub(count).ok_or(Error::Broken(Invariant::EffectsKept))?;
        Ok(())
    }

    pub(crate) const fn promised(&self) -> u32 {
        self.promised
    }

    pub(crate) const fn limit(&self) -> u32 {
        self.limit
    }

    pub(crate) fn within_limit(&self, queued: u64) -> bool {
        self.used(queued) <= u64::from(self.limit)
    }
}

/// `Effect` のキューと予約。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Effects {
    queue: Queue<Effect>,
    seats: Seats,
}

impl Effects {
    /// 上限ぶんの容量を先に確保して作る。以後の `emit` は確保しない。
    pub(crate) fn new(limit: u32) -> Result<Effects, Error> {
        let mut queue = Queue::new();
        let total = usize::try_from(limit).map_err(|_| Error::Exhausted(Resource::HostMemory))?;
        queue.reserve_total(total)?;
        Ok(Effects { queue, seats: Seats::new(limit) })
    }

    fn in_queue(&self) -> u64 {
        u64::try_from(self.queue.len()).unwrap_or(u64::MAX)
    }

    /// `count` 個の席を予約する。
    pub(crate) fn promise(&mut self, count: u32) -> Result<(), Error> {
        self.seats.promise(self.in_queue(), count)
    }

    /// 使わなかった予約を返す。
    pub(crate) fn forgo(&mut self, count: u32) -> Result<(), Error> {
        self.seats.forgo(count)
    }

    /// 予約済みの `Effect` を出す。
    pub(crate) fn emit(&mut self, effect: Effect) -> Result<(), Error> {
        self.seats.forgo(1)?;
        self.queue.push_back_within(effect).map_err(|_| Error::Broken(Invariant::EffectsKept))
    }

    pub(crate) fn peek(&self, at: usize) -> Option<Effect> {
        self.queue.get(at).copied()
    }

    pub(crate) fn poll(&mut self) -> Option<Effect> {
        self.queue.pop_front()
    }

    pub(crate) fn promised(&self) -> u32 {
        self.seats.promised()
    }

    pub(crate) fn queued(&self) -> usize {
        self.queue.len()
    }

    pub(crate) fn within_limit(&self) -> bool {
        self.seats.within_limit(self.in_queue())
    }

    /// 上限ぶんの `Effect` を確保なしで出せる容量があるか。
    pub(crate) fn reserved(&self) -> bool {
        usize::try_from(self.seats.limit()).is_ok_and(|limit| self.queue.capacity() >= limit)
    }

    /// キューの余った容量を捨てる。検査が違反を検出できることを確かめる試験のための入口。
    #[cfg(test)]
    pub(crate) fn corrupt_capacity(&mut self) {
        self.queue.corrupt_capacity();
    }

    /// 上限を書き換える。検査が違反を検出できることを確かめる試験のための入口。
    #[cfg(test)]
    pub(crate) fn corrupt_limit(&mut self, limit: u32) {
        self.seats.limit = limit;
    }

    /// 予約数を書き換える。検査が違反を検出できることを確かめる試験のための入口。
    #[cfg(test)]
    pub(crate) fn corrupt_promised(&mut self, promised: u32) {
        self.seats.promised = promised;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{Arena, kind};

    fn io() -> IoId {
        Arena::<kind::Io, ()>::new(crate::InstanceId::new(1), 1).insert(()).unwrap()
    }

    #[test]
    fn promise_is_refused_before_acceptance_when_full() {
        let mut effects = Effects::new(2).unwrap();
        effects.promise(2).unwrap();
        assert_eq!(effects.promise(1), Err(Error::Exhausted(Resource::Effects)));
        effects.emit(Effect::IoSubmit { io: io() }).unwrap();
        // 出した Effect はホストが取り出すまで席を使い続ける。
        assert_eq!(effects.promise(1), Err(Error::Exhausted(Resource::Effects)));
        assert!(effects.poll().is_some());
        effects.promise(1).unwrap();
    }

    #[test]
    fn promised_effect_is_never_lost() {
        let mut effects = Effects::new(3).unwrap();
        effects.promise(3).unwrap();
        for _ in 0..3 {
            effects.emit(Effect::IoCancel { io: io() }).unwrap();
        }
        assert_eq!(effects.queued(), 3);
        assert_eq!(effects.promised(), 0);
    }

    #[test]
    fn emitting_without_a_promise_is_a_broken_invariant() {
        let mut effects = Effects::new(1).unwrap();
        assert_eq!(effects.emit(Effect::IoSubmit { io: io() }), Err(Error::Broken(Invariant::EffectsKept)));
        assert_eq!(effects.queued(), 0);
    }
}
