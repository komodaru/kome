//! 実行待ちの列: 次に実行権を貸すスレッドを、スレッドの数によらない手数で選ぶ。
//!
//! 列は、(優先度が高い, 先に実行待ちになった) の順で最も前のものが先頭に来るヒープである。
//! 先頭を見るのは一手、出し入れと優先度の変更は列の長さの対数の手数で済む。
//! 各スレッドは列の中の自分の位置を持つので、名指しの取り出しも探さない。
//!
//! 列にあるのは「いま貸せるスレッド」だけである。実行待ちで、停止要求が無く、プロセスに
//! 古い世代の実行が残っていない (台帳 MEM-3) スレッドがちょうど列にある (台帳 RUN-2)。
//! 古い世代の実行が残るプロセスのスレッドは列から外し、残りが無くなった時に、元の順番の
//! まま戻す。

use crate::error::{Error, Invariant};
use crate::id::{ProcessId, ThreadId};
use crate::kernel::Kernel;
use crate::sched::Priority;
use crate::store::plain_try_clone;
use crate::thread::Run;

/// 列の一つの席。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Waiting {
    pub(crate) priority: Priority,
    /// 実行待ちになった順番 ([`Kernel::next_seq`])。
    pub(crate) turn: u64,
    pub(crate) thread: ThreadId,
}

plain_try_clone!(Waiting);

impl Waiting {
    /// `other` より先に貸すか。
    pub(crate) const fn ahead_of(&self, other: &Waiting) -> bool {
        lent_before((self.priority, self.turn), (other.priority, other.turn))
    }
}

/// (優先度, 実行待ちになった順番) の `a` を `b` より先に貸すか。優先度が高い方、同じなら
/// 先に実行待ちになった方。整数だけで決まり、どの二つも必ず順序が付く (Kani が全ての値で示す)。
pub(crate) const fn lent_before(a: (Priority, u64), b: (Priority, u64)) -> bool {
    a.0.get() > b.0.get() || (a.0.get() == b.0.get() && a.1 < b.1)
}

const BROKEN: Error = Error::Broken(Invariant::RunQueue);

impl Kernel {
    /// 実行待ちの末尾の順番を与えて、列へ入れる。容量は [`Kernel::create_thread`] が確保してある。
    pub(crate) fn enqueue(&mut self, thread: ThreadId) -> Result<(), Error> {
        let turn = self.next_seq();
        self.threads.get_mut(thread).ok_or(BROKEN)?.turn = turn;
        self.queue(thread)
    }

    /// 順番を変えずに列へ入れる。古い世代の実行が残るプロセスのスレッドは入れない
    /// (残りが無くなった時に [`Kernel::let_in`] が入れる)。
    fn queue(&mut self, thread: ThreadId) -> Result<(), Error> {
        let entry = self.threads.get(thread).ok_or(BROKEN)?;
        if entry.queued.is_some() {
            return Err(BROKEN);
        }
        let owner = self.processes.get(entry.process).ok_or(Error::Broken(Invariant::ProcessTree))?;
        if owner.stale_runners != 0 {
            return Ok(());
        }
        let at = self.ready.len();
        self.ready.push_within(Waiting { priority: entry.priority, turn: entry.turn, thread }).map_err(|_| BROKEN)?;
        self.place(at)
    }

    /// 列の `at` の席を取り除いて返す。
    pub(crate) fn unqueue(&mut self, at: usize) -> Result<Waiting, Error> {
        let last = self.ready.len().checked_sub(1).ok_or(BROKEN)?;
        self.ready.swap(at, last).ok_or(BROKEN)?;
        let gone = self.ready.pop().ok_or(BROKEN)?;
        // 終了の途中では、スレッドの記録は既に無い。
        if let Some(entry) = self.threads.get_mut(gone.thread) {
            entry.queued = None;
        }
        if at < last {
            self.place(at)?;
        }
        Ok(gone)
    }

    /// 貸せる状態だったスレッドを列から外す。`queued` はそのスレッドが持っていた位置。
    /// 位置が無いのは、プロセスに古い世代の実行が残っていて列から外してあるときだけである。
    pub(crate) fn leave(&mut self, thread: ThreadId, process: ProcessId, queued: Option<usize>) -> Result<(), Error> {
        match queued {
            Some(at) => {
                if self.unqueue(at)?.thread == thread { Ok(()) } else { Err(BROKEN) }
            }
            None => {
                let owner = self.processes.get(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
                if owner.stale_runners == 0 { Err(BROKEN) } else { Ok(()) }
            }
        }
    }

    /// 列にあるスレッドの優先度を変え、席を正しい位置へ動かす。
    pub(crate) fn requeue(&mut self, at: usize, priority: Priority) -> Result<(), Error> {
        self.ready.get_mut(at).ok_or(BROKEN)?.priority = priority;
        self.place(at)
    }

    /// `process` に古い世代の実行が残っている間、そのスレッドを列から外す。確保しない。
    pub(crate) fn hold_back(&mut self, process: ProcessId) -> Result<(), Error> {
        let mut member = 0usize;
        while let Some(thread) = self.member(process, member)? {
            member = member.saturating_add(1);
            if let Some(at) = self.threads.get(thread).ok_or(Error::Broken(Invariant::ProcessTree))?.queued {
                self.unqueue(at)?;
            }
        }
        Ok(())
    }

    /// 古い世代の実行が無くなった `process` の、貸せるスレッドを元の順番のまま列へ戻す。
    pub(crate) fn let_in(&mut self, process: ProcessId) -> Result<(), Error> {
        let mut member = 0usize;
        while let Some(thread) = self.member(process, member)? {
            member = member.saturating_add(1);
            let entry = self.threads.get(thread).ok_or(Error::Broken(Invariant::ProcessTree))?;
            if entry.run == Run::Ready && entry.suspend == 0 && entry.queued.is_none() {
                self.queue(thread)?;
            }
        }
        Ok(())
    }

    fn member(&self, process: ProcessId, at: usize) -> Result<Option<ThreadId>, Error> {
        let owner = self.processes.get(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
        Ok(owner.threads.get(at).copied())
    }

    /// `at` の席を、親より先なら上へ、子より後なら下へ動かし、動いた席の位置を記録する。
    fn place(&mut self, at: usize) -> Result<(), Error> {
        let at = self.rise(at)?;
        self.sink(at)
    }

    fn rise(&mut self, mut at: usize) -> Result<usize, Error> {
        // 木の深さは usize のビット数を超えない。
        for _ in 0..usize::BITS {
            let Some(parent) = at.checked_sub(1).map(|below| below >> 1) else { break };
            if !self.ahead(at, parent)? {
                break;
            }
            self.ready.swap(at, parent).ok_or(BROKEN)?;
            self.seat(at)?;
            at = parent;
        }
        self.seat(at)?;
        Ok(at)
    }

    fn sink(&mut self, mut at: usize) -> Result<(), Error> {
        for _ in 0..usize::BITS {
            let left = at.saturating_mul(2).saturating_add(1);
            let right = left.saturating_add(1);
            let mut first = at;
            if left < self.ready.len() && self.ahead(left, first)? {
                first = left;
            }
            if right < self.ready.len() && self.ahead(right, first)? {
                first = right;
            }
            if first == at {
                break;
            }
            self.ready.swap(at, first).ok_or(BROKEN)?;
            self.seat(at)?;
            at = first;
        }
        self.seat(at)
    }

    /// 列の `a` の席を `b` の席より先に貸すか。
    fn ahead(&self, a: usize, b: usize) -> Result<bool, Error> {
        let a = self.ready.get(a).ok_or(BROKEN)?;
        let b = self.ready.get(b).ok_or(BROKEN)?;
        Ok(a.ahead_of(b))
    }

    /// `at` の席のスレッドへ、列の中の位置を記録する。
    fn seat(&mut self, at: usize) -> Result<(), Error> {
        let thread = self.ready.get(at).ok_or(BROKEN)?.thread;
        self.threads.get_mut(thread).ok_or(BROKEN)?.queued = Some(at);
        Ok(())
    }
}
