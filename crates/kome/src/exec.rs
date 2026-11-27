//! 実行器の契約: ゲスト命令を実行するもの (インタプリタ、JIT、ネイティブ実行) と kome の境界。
//!
//! kome は実行器の中身を知らない。決めるのは次の約束だけである。
//!
//! 1. 実行器は、実行権 ([`RunPermit`]) を借りている間だけ、そのスレッドを実行する。
//! 2. 実行器は区切りごとに、実行した量と中断の要求を確かめる。実行量を使い切ったら
//!    [`Stop::BudgetExhausted`]、中断を要求されていたら [`Stop::Interrupted`] で止まる。
//! 3. 実行器が使うのは、実行権が持つアドレス空間の世代 ([`RunPermit::map`]) で検証した
//!    マッピングだけである。
//! 4. 止まったとき、スレッドの続きを実行するのに要るものは全て文脈 ([`Executor::Context`]) に
//!    入っている。実行器の中にも worker の中にも残さない。だから次の実行は、どの worker が、
//!    どの実行器の値で行ってもよい (少ない worker で多くのスレッドを実行できる)。
//!
//! 文脈は [`Contexts`] が持つ。実行の間だけ [`Contexts::load`] で取り出し、止まったら
//! [`Contexts::save`] で戻す。取り出している文脈 ([`Loaded`]) は複製できず、二つの worker が
//! 同じスレッドの文脈を同時に持つことは無い。下の二つは `u8` (複製できる) か `Loaded` かだけが違う。
//!
//! ```
//! fn twice(context: u8) -> (u8, u8) { (context.clone(), context) }
//! ```
//!
//! ```compile_fail
//! fn twice(context: kome::Loaded<u8>) -> (kome::Loaded<u8>, kome::Loaded<u8>) { (context.clone(), context) }
//! ```
//!
//! [`run_once`] は、この約束どおりの一回ぶん (借りる、載せる、実行する、降ろす、返す) である。
//! worker が一つのホストはこれを繰り返す。worker ごとにホストのスレッドを持つホストは、
//! 「載せる、実行する、降ろす」を worker の側で行い、実行権と止まった理由を単一 writer へ送る。

use crate::error::{Error, Refusal};
use crate::id::ThreadId;
use crate::kernel::Kernel;
use crate::sched::{Budget, NotRelinquished, RunGen, RunPermit, Stop, ThreadFate, WorkerId};
use crate::store::List;

/// 実行器が止まった理由と、実行した量。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stopped {
    stop: Stop,
    consumed: Budget,
}

impl Stopped {
    /// 理由と実行した量から作る。
    #[must_use]
    pub const fn new(stop: Stop, consumed: Budget) -> Stopped {
        Stopped { stop, consumed }
    }

    /// 止まった理由。
    #[must_use]
    pub const fn stop(&self) -> Stop {
        self.stop
    }

    /// 実行した量。
    #[must_use]
    pub const fn consumed(&self) -> Budget {
        self.consumed
    }
}

/// ゲスト命令を実行するもの。
pub trait Executor {
    /// スレッドの続きを実行するのに要るもの全て (レジスタ、実行位置など)。
    type Context;

    /// `context` の続きを、実行権の範囲で実行し、止まった理由と実行した量を返す。
    ///
    /// 戻るとき、続きの実行に要るものは全て `context` にある。
    fn run(&mut self, permit: &RunPermit, context: &mut Self::Context) -> Stopped;
}

#[derive(Debug)]
enum Seat<C> {
    /// 実行していない。文脈はここにある。
    Parked(C),
    /// この実行のために取り出している。
    Loaded(RunGen),
}

#[derive(Debug)]
struct Slot<C> {
    thread: ThreadId,
    seat: Seat<C>,
}

/// 実行のために取り出した、一つのスレッドの文脈。複製できない。[`Contexts::save`] で戻す。
#[must_use = "取り出した文脈は Contexts::save で戻さなければならない"]
#[derive(Debug)]
pub struct Loaded<C> {
    thread: ThreadId,
    run: RunGen,
    context: C,
}

impl<C> Loaded<C> {
    /// 文脈の持ち主のスレッド。
    #[must_use]
    pub const fn thread(&self) -> ThreadId {
        self.thread
    }

    /// 文脈。
    pub const fn context(&mut self) -> &mut C {
        &mut self.context
    }
}

/// スレッドごとの文脈の置き場。
///
/// スレッドを作ったら [`Contexts::insert`]、終了の通知 ([`crate::Effect::ThreadExited`]) を
/// 受けたら [`Contexts::remove`] を呼ぶ。
#[derive(Debug)]
pub struct Contexts<C> {
    slots: List<Slot<C>>,
}

impl<C> Default for Contexts<C> {
    fn default() -> Contexts<C> {
        Contexts::new()
    }
}

impl<C> Contexts<C> {
    /// 空の置き場。
    #[must_use]
    pub const fn new() -> Contexts<C> {
        Contexts { slots: List::new() }
    }

    /// 文脈を持っているスレッドの数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// 一つも持っていないか。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    fn slot(&mut self, thread: ThreadId) -> Result<&mut Slot<C>, Error> {
        self.slots.iter_mut().find(|slot| slot.thread == thread).ok_or(Error::Refused(Refusal::NoContext))
    }

    /// `thread` の最初の文脈を置く。既にあれば `Refused(ContextBusy)`。
    pub fn insert(&mut self, thread: ThreadId, context: C) -> Result<(), Error> {
        if self.slots.iter().any(|slot| slot.thread == thread) {
            return Err(Error::Refused(Refusal::ContextBusy));
        }
        self.slots.push(Slot { thread, seat: Seat::Parked(context) })
    }

    /// 実行権のスレッドの文脈を取り出す。
    ///
    /// 文脈が無ければ `Refused(NoContext)`、既に取り出されていれば `Refused(ContextBusy)`。
    pub fn load(&mut self, permit: &RunPermit) -> Result<Loaded<C>, Error> {
        let thread = permit.thread();
        let slot = self.slot(thread)?;
        match core::mem::replace(&mut slot.seat, Seat::Loaded(permit.run())) {
            Seat::Parked(context) => Ok(Loaded { thread, run: permit.run(), context }),
            Seat::Loaded(earlier) => {
                slot.seat = Seat::Loaded(earlier);
                Err(Error::Refused(Refusal::ContextBusy))
            }
        }
    }

    /// 取り出した文脈を戻す。
    ///
    /// その実行のために取り出した記録が無ければ (スレッドの文脈を取り除いた後など)
    /// `Refused(NoContext)` を返し、文脈は捨てる。
    pub fn save(&mut self, loaded: Loaded<C>) -> Result<(), Error> {
        let Loaded { thread, run, context } = loaded;
        let slot = self.slot(thread)?;
        match &slot.seat {
            Seat::Loaded(taken) if *taken == run => {
                slot.seat = Seat::Parked(context);
                Ok(())
            }
            Seat::Loaded(_) | Seat::Parked(_) => Err(Error::Refused(Refusal::NoContext)),
        }
    }

    /// `thread` の文脈を取り除いて返す。実行のために取り出している間は、記録だけを取り除いて
    /// `None` を返す (後の [`Contexts::save`] は `Refused(NoContext)` になり、文脈は捨てられる)。
    pub fn remove(&mut self, thread: ThreadId) -> Result<Option<C>, Error> {
        let slot = self.slots.remove_first(|slot| slot.thread == thread).ok_or(Error::Refused(Refusal::NoContext))?;
        Ok(match slot.seat {
            Seat::Parked(context) => Some(context),
            Seat::Loaded(_) => None,
        })
    }
}

/// 一回の実行の結果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ran {
    thread: ThreadId,
    stopped: Stopped,
    fate: ThreadFate,
}

impl Ran {
    /// 実行したスレッド。
    #[must_use]
    pub const fn thread(&self) -> ThreadId {
        self.thread
    }

    /// 止まった理由と、実行した量。
    #[must_use]
    pub const fn stopped(&self) -> Stopped {
        self.stopped
    }

    /// 実行権を返した後のスレッド。[`ThreadFate::Servicing`] なら、Personality が理由を見て処理する。
    #[must_use]
    pub const fn fate(&self) -> ThreadFate {
        self.fate
    }
}

/// worker の一回ぶん: 実行権を借り、文脈を載せ、実行し、文脈を降ろし、実行権を返す。
/// 貸せるスレッドが無ければ `None`。
pub fn run_once<E: Executor>(
    kernel: &mut Kernel,
    worker: WorkerId,
    executor: &mut E,
    contexts: &mut Contexts<E::Context>,
) -> Result<Option<Ran>, Error> {
    match kernel.grant(worker)? {
        Some(permit) => run_permit(kernel, permit, executor, contexts).map(Some),
        None => Ok(None),
    }
}

/// 借りた実行権で一回実行する。スレッドを名指しして借りた実行権 ([`Kernel::grant_thread`]) にも使う。
///
/// 失敗を返すときも、実行権は必ず返している: 文脈が無い、または実行器が状況と合わない理由を
/// 返したときは、スレッドをサービス中にして ([`Stop::Trap`])、その失敗を返す。スレッドが終了したら
/// 文脈を取り除く。
pub fn run_permit<E: Executor>(
    kernel: &mut Kernel,
    permit: RunPermit,
    executor: &mut E,
    contexts: &mut Contexts<E::Context>,
) -> Result<Ran, Error> {
    let thread = permit.thread();
    let mut loaded = match contexts.load(&permit) {
        Ok(loaded) => loaded,
        Err(error) => return Err(hand_back(kernel, permit, Budget::new(0)).err().unwrap_or(error)),
    };
    let stopped = executor.run(&permit, loaded.context());
    if let Err(error) = contexts.save(loaded) {
        return Err(hand_back(kernel, permit, stopped.consumed).err().unwrap_or(error));
    }
    let fate = match kernel.relinquish(permit, stopped.stop, stopped.consumed) {
        Ok(fate) => fate,
        Err(NotRelinquished::Rejected(permit, error)) => {
            return Err(hand_back(kernel, permit, stopped.consumed).err().unwrap_or(error));
        }
        Err(NotRelinquished::Failed(error)) => return Err(error),
    };
    match fate {
        ThreadFate::Exited(_) => {
            contexts.remove(thread)?;
        }
        ThreadFate::Ready | ThreadFate::Servicing => {}
    }
    Ok(Ran { thread, stopped, fate })
}

/// 続きを実行できなくなった実行権を返し、スレッドをサービス中にする。
fn hand_back(kernel: &mut Kernel, permit: RunPermit, consumed: Budget) -> Result<(), Error> {
    match kernel.relinquish(permit, Stop::Trap, consumed) {
        Ok(_) => Ok(()),
        Err(refused) => Err(refused.error()),
    }
}
