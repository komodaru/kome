//! 待機: 条件の確認と登録を一つの状態更新で行い、結果を一つだけ確定する。
//!
//! 待機は対象の列、成立条件 (どれか一つ / すべて)、期限、割り込みを受けるかどうかを
//! 持つ。確認と登録の間に通知が失われることは無い (同じ `&mut` の中で行う)。
//!
//! 「すべて」の待機は、全対象が同時に成立したときにだけ、まとめて消費する。対象を
//! 一つずつ先に取ることはしない。「どれか一つ」は、成立している対象のうち列の先頭に
//! 近いものを選ぶ。同じ資源を待つ待機は登録順に評価する。
//!
//! 対象の無い待機は成立しない。期限だけで終わる (一定時間の休止)。

use crate::effect::Effect;
use crate::error::{Error, Invariant, Refusal, Resource};
use crate::id::{ObjectId, ThreadId};
use crate::kernel::{Due, Kernel};
use crate::object::SignalMode;
use crate::store::{List, TryClone};
use crate::thread::Run;
use crate::time::{Deadline, Instant};

/// 待機の成立条件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitMode {
    /// 対象のどれか一つの数が 1 以上になれば成立する。
    Any,
    /// 対象すべての数が同時に 1 以上になれば成立する。
    All,
}

/// 待機が [`Kernel::interrupt_wait`] による中断を受け付けるか。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interruptible {
    /// 受け付ける。
    Yes,
    /// 受け付けない。
    No,
}

/// 待機の結果。一つの待機に一つだけ確定する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitOutcome {
    /// 「どれか一つ」が成立した。`index` は対象の列での位置。
    Signaled {
        /// 成立した対象の位置。
        index: u32,
    },
    /// 「すべて」が成立した。
    AllSignaled,
    /// 期限が来た。
    TimedOut,
    /// [`Kernel::interrupt_wait`] で中断された。
    Interrupted,
}

/// [`Kernel::wait`] の返答。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitStart {
    /// 登録せずに結果が確定した。スレッドはサービス中のままである。
    Done(WaitOutcome),
    /// 登録した。結果は [`Effect::WaitDone`] で確定する。
    Blocked,
}

impl TryClone for Wait {
    fn try_clone(&self) -> Result<Wait, Error> {
        let Wait { targets, mode, due, interruptible } = self;
        Ok(Wait { targets: targets.try_clone()?, mode: *mode, due: *due, interruptible: *interruptible })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Wait {
    pub(crate) targets: List<ObjectId>,
    pub(crate) mode: WaitMode,
    /// 期限と、期限の列での登録順。
    pub(crate) due: Option<(Instant, u64)>,
    pub(crate) interruptible: Interruptible,
}

impl Kernel {
    /// サービス中のスレッドの待機を、確認と登録を分けずに始める。
    ///
    /// 既に成立していれば消費して結果を返す。期限が現在以前なら時間切れを返す。
    /// どちらでもなければ登録し、スレッドは待機中になる。
    pub fn wait(
        &mut self,
        thread: ThreadId,
        targets: &[ObjectId],
        mode: WaitMode,
        deadline: Deadline,
        interruptible: Interruptible,
    ) -> Result<WaitStart, Error> {
        let entry = self.threads.find(thread, &self.lineage)?;
        if entry.run != Run::Servicing {
            return Err(Error::Refused(Refusal::NotServicing));
        }
        if u64::try_from(targets.len()).unwrap_or(u64::MAX) > u64::from(self.limits.wait_targets()) {
            return Err(Error::Exhausted(Resource::WaitTargets));
        }
        for (at, target) in targets.iter().enumerate() {
            self.can_pin(*target, 1)?;
            if targets.iter().take(at).any(|earlier| earlier == target) {
                return Err(Error::Refused(Refusal::DuplicateTarget));
            }
        }
        if let Some(outcome) = self.evaluate(targets, mode)? {
            self.consume(targets, outcome)?;
            return Ok(WaitStart::Done(outcome));
        }
        let due = match deadline {
            Deadline::At(at) if at <= self.now => return Ok(WaitStart::Done(WaitOutcome::TimedOut)),
            Deadline::At(at) => Some(at),
            Deadline::Never => None,
        };
        // ここから下は予約。失敗しても状態は変わらない。
        let mut list = List::new();
        list.reserve(targets.len())?;
        for target in targets {
            self.objects.get_mut(*target).ok_or(Error::Broken(Invariant::WaitOnce))?.waiters.reserve(1)?;
        }
        if due.is_some() {
            self.due.reserve(1)?;
        }
        self.effects.promise(1)?;
        // ここから下は確定。
        for target in targets {
            list.push(*target)?;
            self.pin(*target)?;
            self.objects.get_mut(*target).ok_or(Error::Broken(Invariant::WaitOnce))?.waiters.push(thread)?;
        }
        let due = match due {
            Some(at) => {
                let seq = self.next_seq();
                self.due_insert(at, seq, Due::Wait(thread))?;
                Some((at, seq))
            }
            None => None,
        };
        let entry = self.threads.get_mut(thread).ok_or(Error::Broken(Invariant::WaitOnce))?;
        entry.run = Run::Blocked(Wait { targets: list, mode, due, interruptible });
        Ok(WaitStart::Blocked)
    }

    /// 割り込みを受け付ける待機を中断する。スレッドはサービス中に戻る。
    ///
    /// 結果 ([`WaitOutcome::Interrupted`]) はこの呼び出しの成功で確定し、`Effect` は出さない。
    pub fn interrupt_wait(&mut self, thread: ThreadId) -> Result<(), Error> {
        let entry = self.threads.find_mut(thread, &self.lineage)?;
        match &entry.run {
            Run::Blocked(wait) if wait.interruptible == Interruptible::Yes => {}
            Run::Blocked(_) => return Err(Error::Refused(Refusal::NotInterruptible)),
            Run::Ready | Run::Running { .. } | Run::Servicing => return Err(Error::Refused(Refusal::NotWaiting)),
        }
        let Run::Blocked(wait) = core::mem::replace(&mut entry.run, Run::Servicing) else {
            return Err(Error::Broken(Invariant::WaitOnce));
        };
        self.detach_wait(thread, &wait)?;
        self.effects.forgo(1)
    }

    /// いま成立しているなら、その結果。
    pub(crate) fn evaluate(&self, targets: &[ObjectId], mode: WaitMode) -> Result<Option<WaitOutcome>, Error> {
        let mut first = None;
        let mut all = !targets.is_empty();
        for (index, target) in (0u32..=u32::MAX).zip(targets) {
            let object = self.objects.get(*target).ok_or(Error::Broken(Invariant::WaitOnce))?;
            if object.count == 0 {
                all = false;
            } else if first.is_none() {
                first = Some(index);
            }
        }
        Ok(match mode {
            WaitMode::Any => first.map(|index| WaitOutcome::Signaled { index }),
            WaitMode::All => all.then_some(WaitOutcome::AllSignaled),
        })
    }

    /// 成立した待機が対象の数を消費する。
    fn consume(&mut self, targets: &[ObjectId], outcome: WaitOutcome) -> Result<(), Error> {
        let chosen = match outcome {
            WaitOutcome::Signaled { index } => {
                let at = usize::try_from(index).map_err(|_| Error::Broken(Invariant::WaitOnce))?;
                targets.get(at..=at).ok_or(Error::Broken(Invariant::WaitOnce))?
            }
            WaitOutcome::AllSignaled => targets,
            WaitOutcome::TimedOut | WaitOutcome::Interrupted => &[],
        };
        for target in chosen {
            let object = self.objects.get_mut(*target).ok_or(Error::Broken(Invariant::WaitOnce))?;
            match object.mode {
                SignalMode::Keep => {}
                SignalMode::Consume => {
                    object.count = object.count.checked_sub(1).ok_or(Error::Broken(Invariant::WaitOnce))?;
                }
            }
        }
        Ok(())
    }

    /// 待機の登録を、対象の待機列・期限の列・固定から外す。確保しない。
    pub(crate) fn detach_wait(&mut self, thread: ThreadId, wait: &Wait) -> Result<(), Error> {
        for target in wait.targets.iter() {
            let object = self.objects.get_mut(*target).ok_or(Error::Broken(Invariant::WaitOnce))?;
            object.waiters.remove_first(|waiter| *waiter == thread).ok_or(Error::Broken(Invariant::WaitOnce))?;
        }
        if let Some((at, seq)) = wait.due {
            self.due_remove(at, seq, Due::Wait(thread))?;
        }
        for target in wait.targets.iter() {
            self.unpin(*target)?;
        }
        Ok(())
    }

    /// 登録済みの待機の結果を確定する。スレッドはサービス中に戻る。
    pub(crate) fn finish_wait(&mut self, thread: ThreadId, outcome: WaitOutcome) -> Result<(), Error> {
        let entry = self.threads.get_mut(thread).ok_or(Error::Broken(Invariant::WaitOnce))?;
        if !matches!(entry.run, Run::Blocked(_)) {
            return Err(Error::Broken(Invariant::WaitOnce));
        }
        let Run::Blocked(wait) = core::mem::replace(&mut entry.run, Run::Servicing) else {
            return Err(Error::Broken(Invariant::WaitOnce));
        };
        self.consume(wait.targets.as_slice(), outcome)?;
        // 結果の通知を、対象の破棄の通知より先に出す。
        self.effects.emit(Effect::WaitDone { thread, outcome })?;
        self.detach_wait(thread, &wait)
    }

    /// 資源の数が変わった後、その資源を待つ待機を登録順に評価して確定する。
    pub(crate) fn settle(&mut self, object: ObjectId) -> Result<(), Error> {
        let mut at = 0usize;
        loop {
            // 待機が外れると資源が破棄されることがある。そのときは評価する相手がもう無い。
            let Some(entry) = self.objects.get(object) else { return Ok(()) };
            if entry.count == 0 {
                return Ok(());
            }
            let Some(thread) = entry.waiters.get(at).copied() else { return Ok(()) };
            let waiter = self.threads.get(thread).ok_or(Error::Broken(Invariant::WaitOnce))?;
            let Run::Blocked(wait) = &waiter.run else { return Err(Error::Broken(Invariant::WaitOnce)) };
            match self.evaluate(wait.targets.as_slice(), wait.mode)? {
                Some(outcome) => {
                    self.finish_wait(thread, outcome)?;
                    // 確定した待機は待機列から外れている。残っていれば同じ待機を繰り返すので止める。
                    let listed = self.objects.get(object).is_some_and(|o| o.waiters.iter().any(|w| *w == thread));
                    if listed {
                        return Err(Error::Broken(Invariant::WaitOnce));
                    }
                }
                None => at = at.saturating_add(1),
            }
        }
    }
}
