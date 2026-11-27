//! 仮想時刻、期限、タイマ。
//!
//! kome は時計を読まない。[`Kernel::advance`] が渡された時刻まで、期限の来た待機と
//! タイマを (時刻, 登録順) の順に一つずつ処理する。ホストは [`Kernel::next_deadline`]
//! を見て次に起こす時刻を決める。試験は実時間を待たず、次の期限へ時刻を進める。
//!
//! タイマは待機可能な資源に結び付き、期限が来るとその数を 1 増やす (上限で止まる)。
//! 周期の下限は [`crate::Limits::min_period`] である。
//!
//! 一度の `advance` が処理する期限の数には目安 ([`crate::Limits::advance_batch`]) がある。
//! 時刻を大きく進めると、周期タイマは過ぎた回数だけ通知する。その数は進めた長さに比例するので、
//! `advance` は目安の数を処理したところで返り、どこまで進んだかと、まだ残っているかを返す
//! ([`Advanced`])。呼び出し側は、残っている間、同じ時刻でもう一度呼ぶ。通知をまとめたり
//! 間引いたりはしない。待機の起床の順序と、消費する資源の数が変わってしまうからである。

use crate::error::{Error, Invariant, Refusal};
use crate::id::{ObjectId, ProcessId, TimerId};
use crate::kernel::{Due, DueEntry, Kernel};
use crate::process::Life;
use crate::store::plain_try_clone;
use crate::time::{Instant, Period};
use crate::wait::WaitOutcome;

/// [`Kernel::advance`] の結果。
#[must_use = "more() が真なら、まだ期限が残っている。同じ時刻でもう一度 advance を呼ぶ"]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Advanced {
    fired: u32,
    reached: Instant,
    more: bool,
}

impl Advanced {
    /// この呼び出しで処理した期限の数。
    #[must_use]
    pub const fn fired(&self) -> u32 {
        self.fired
    }

    /// 進んだ先の仮想時刻。[`Advanced::more`] が偽なら、渡した時刻と同じ。
    #[must_use]
    pub const fn reached(&self) -> Instant {
        self.reached
    }

    /// 渡した時刻までに処理する期限が、まだ残っているか。
    #[must_use]
    pub const fn more(&self) -> bool {
        self.more
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Armed {
    pub(crate) at: Instant,
    pub(crate) seq: u64,
    pub(crate) period: Option<Period>,
}

plain_try_clone!(Timer);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Timer {
    pub(crate) process: ProcessId,
    pub(crate) object: ObjectId,
    pub(crate) armed: Option<Armed>,
}

impl Kernel {
    /// `object` へ通知するタイマを、止まった状態で作る。
    pub fn create_timer(&mut self, process: ProcessId, object: ObjectId) -> Result<TimerId, Error> {
        let owner = self.processes.find(process, &self.lineage)?;
        if owner.life != Life::Alive {
            return Err(Error::Refused(Refusal::ProcessExiting));
        }
        self.can_pin(object, 1)?;
        self.timers.reserve()?;
        let timer = self.timers.insert(Timer { process, object, armed: None })?;
        self.pin(object)?;
        Ok(timer)
    }

    /// タイマを `at` に設定する。`period` があれば、以後その間隔で繰り返す。
    /// 周期は 0 にできず ([`Period`])、[`crate::Limits::min_period`] より短ければ拒否する。
    /// 設定済みなら置き換える。`at` が現在以前なら、次の [`Kernel::advance`] で通知する。
    pub fn arm_timer(&mut self, timer: TimerId, at: Instant, period: Option<Period>) -> Result<(), Error> {
        let entry = self.timers.find(timer, &self.lineage)?;
        if period.is_some_and(|period| period < self.limits.min_period()) {
            return Err(Error::Refused(Refusal::PeriodTooShort));
        }
        let previous = entry.armed;
        self.due.reserve(1)?;
        if let Some(armed) = previous {
            self.due_remove(armed.at, armed.seq, Due::Timer(timer))?;
        }
        let seq = self.next_seq();
        self.due_insert(at, seq, Due::Timer(timer))?;
        self.timers.get_mut(timer).ok_or(Error::Broken(Invariant::TimeOrder))?.armed = Some(Armed { at, seq, period });
        Ok(())
    }

    /// タイマを止める。設定されていたかを返す。既に増やした数は戻さない。
    pub fn disarm_timer(&mut self, timer: TimerId) -> Result<bool, Error> {
        let entry = self.timers.find_mut(timer, &self.lineage)?;
        let Some(armed) = entry.armed.take() else { return Ok(false) };
        self.due_remove(armed.at, armed.seq, Due::Timer(timer))?;
        Ok(true)
    }

    /// タイマを破棄する。
    pub fn drop_timer(&mut self, timer: TimerId) -> Result<(), Error> {
        let entry = self.timers.take(timer, &self.lineage)?;
        if let Some(armed) = entry.armed {
            self.due_remove(armed.at, armed.seq, Due::Timer(timer))?;
        }
        self.unpin(entry.object)
    }

    /// 仮想時刻を `to` へ進め、期限の来た待機とタイマを (時刻, 登録順) の順に処理する。
    ///
    /// 処理した数が [`crate::Limits::advance_batch`] に達したら、同じ時刻の期限を処理し終えた
    /// ところで返る。そのとき時刻は最後に処理した期限の時刻までしか進まず (過去の時刻に設定された
    /// タイマの分を処理している間は現在のまま)、[`Advanced::more`] が
    /// 真になる。残りは、同じ `to` でもう一度呼ぶと続きから処理する。
    ///
    /// 一度の呼び出しの仕事は、目安の数に、一つの時刻に並ぶ期限の数 (待機とタイマの数以下) を
    /// 足した数を超えない。
    pub fn advance(&mut self, to: Instant) -> Result<Advanced, Error> {
        if to < self.now {
            return Err(Error::Refused(Refusal::TimeReversed));
        }
        let batch = self.limits.advance_batch().get();
        let mut fired = 0u32;
        // 直前に処理した期限の時刻 (設定された時刻。過去に設定されたタイマでは現在より前になる)。
        let mut last: Option<Instant> = None;
        while let Some(entry) = self.due.get(0).copied().filter(|entry| entry.at <= to) {
            // 区切ってよいのは、次の期限の時刻が直前に処理した期限と違うときだけである。同じ時刻の
            // 途中で区切ると、同時に期限が来た待機の一部だけを起こして返ることになる。周期は 0 で
            // ないので、一つのタイマが同じ時刻に並び続けることはない。
            if fired >= batch && last != Some(entry.at) {
                return Ok(Advanced { fired, reached: self.now, more: true });
            }
            last = Some(entry.at);
            // 先頭を外せなければ同じ期限を繰り返すことになるので、内部の障害として止める。
            if self.due.remove(0).is_none() {
                return Err(Error::Broken(Invariant::TimeOrder));
            }
            // 過去の時刻に設定されたタイマは、現在の時刻で通知する。
            self.now = self.now.max(entry.at);
            fired = fired.saturating_add(1);
            match entry.what {
                Due::Wait(thread) => {
                    // 期限の列からは外し終えているので、待機の側の記録を消してから確定する。
                    let waiter = self.threads.get_mut(thread).ok_or(Error::Broken(Invariant::TimeOrder))?;
                    match &mut waiter.run {
                        crate::thread::Run::Blocked(wait) if wait.due == Some((entry.at, entry.seq)) => wait.due = None,
                        crate::thread::Run::Blocked(_)
                        | crate::thread::Run::Ready
                        | crate::thread::Run::Running { .. }
                        | crate::thread::Run::Servicing => return Err(Error::Broken(Invariant::TimeOrder)),
                    }
                    self.finish_wait(thread, WaitOutcome::TimedOut)?;
                }
                Due::Timer(timer) => self.fire(timer, entry)?,
            }
        }
        self.now = to;
        Ok(Advanced { fired, reached: to, more: false })
    }

    /// 最も早い期限。無ければ `None`。
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.due.get(0).map(|entry| entry.at)
    }

    fn fire(&mut self, timer: TimerId, entry: DueEntry) -> Result<(), Error> {
        let record = self.timers.get(timer).ok_or(Error::Broken(Invariant::TimeOrder))?;
        let object = record.object;
        let Some(armed) = record.armed.filter(|armed| armed.at == entry.at && armed.seq == entry.seq) else {
            return Err(Error::Broken(Invariant::TimeOrder));
        };
        // 周期の次回。周期は 0 でないので、次回は必ず今回より後になる (証明 a_period_moves_the_deadline_forward)。時刻が表現できなくなったら
        // 止まる。列からは一つ外したばかりなので確保しない。
        let next = armed.period.and_then(|period| armed.at.checked_add(period.get()));
        let rearmed = match next {
            Some(at) => {
                let seq = self.next_seq();
                self.due_insert(at, seq, Due::Timer(timer))?;
                Some(Armed { at, seq, period: armed.period })
            }
            None => None,
        };
        self.timers.get_mut(timer).ok_or(Error::Broken(Invariant::TimeOrder))?.armed = rearmed;
        self.raise(object)
    }

    /// 終了するプロセスのタイマをすべて破棄する。
    pub(crate) fn drop_timers_of(&mut self, process: ProcessId) -> Result<(), Error> {
        loop {
            let found = self.timers.iter().find(|(_, timer)| timer.process == process).map(|(id, _)| id);
            let Some(timer) = found else { return Ok(()) };
            self.drop_timer(timer).map_err(|_| Error::Broken(Invariant::TimeOrder))?;
            if self.timers.contains(timer) {
                return Err(Error::Broken(Invariant::TimeOrder));
            }
        }
    }

    /// 期限の列へ (時刻, 登録順) の位置で入れる。
    pub(crate) fn due_insert(&mut self, at: Instant, seq: u64, what: Due) -> Result<(), Error> {
        let place = self.due.as_slice().partition_point(|entry| (entry.at, entry.seq) <= (at, seq));
        self.due.insert(place, DueEntry { at, seq, what }, Invariant::TimeOrder)
    }

    /// 期限の列から一項目を外す。確保しない。
    pub(crate) fn due_remove(&mut self, at: Instant, seq: u64, what: Due) -> Result<(), Error> {
        self.due
            .remove_first(|entry| entry.at == at && entry.seq == seq && entry.what == what)
            .map(|_| ())
            .ok_or(Error::Broken(Invariant::TimeOrder))
    }
}
