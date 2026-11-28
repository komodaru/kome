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

use crate::error::{Error, Refusal, Resource};
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
    /// スレッドのスロット番号で引く表。文脈を持たないスロットは空いている。
    slots: List<Option<Slot<C>>>,
    held: usize,
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
        Contexts { slots: List::new(), held: 0 }
    }

    /// 文脈を持っているスレッドの数。
    #[must_use]
    pub const fn len(&self) -> usize {
        self.held
    }

    /// 一つも持っていないか。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.held == 0
    }

    /// `thread` のスロットの席。スレッドの数によらず、一度で引く。
    fn seat(&mut self, thread: ThreadId) -> Option<&mut Option<Slot<C>>> {
        self.slots.get_mut(usize::try_from(thread.slot()).ok()?)
    }

    fn slot(&mut self, thread: ThreadId) -> Result<&mut Slot<C>, Error> {
        let slot = self.seat(thread).and_then(Option::as_mut).filter(|slot| slot.thread == thread);
        slot.ok_or(Error::Refused(Refusal::NoContext))
    }

    /// `thread` の最初の文脈を置く。既にあれば `Refused(ContextBusy)`。
    ///
    /// 同じスロットを使っていた前のスレッドの文脈が残っているときも `Refused(ContextBusy)` を返す
    /// (黙って捨てない)。終了したスレッドの文脈は [`Contexts::remove`] で取り除いておく。
    pub fn insert(&mut self, thread: ThreadId, context: C) -> Result<(), Error> {
        let at = usize::try_from(thread.slot()).map_err(|_| Error::Exhausted(Resource::Identifiers))?;
        let missing = at.saturating_add(1).saturating_sub(self.slots.len());
        self.slots.reserve(missing)?;
        while self.slots.len() <= at {
            self.slots.push(None)?;
        }
        match self.slots.get_mut(at) {
            Some(seat @ None) => {
                *seat = Some(Slot { thread, seat: Seat::Parked(context) });
                self.held = self.held.saturating_add(1);
                Ok(())
            }
            Some(Some(_)) | None => Err(Error::Refused(Refusal::ContextBusy)),
        }
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
        self.slot(thread)?;
        let slot = self.seat(thread).and_then(Option::take).ok_or(Error::Refused(Refusal::NoContext))?;
        self.held = self.held.saturating_sub(1);
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

/// 実行を始められなかった、または終えられなかった理由。
///
/// 実行権は複製できないだけでなく、失敗を返すときに失ってもならない。この型は、`Kernel` が
/// 受け取らなかった実行権と文脈を、呼び出した側へそのまま返す。
#[must_use = "拒否された実行権は into_permit で取り戻す"]
#[derive(Debug)]
pub enum NotRun<C> {
    /// この `Kernel` が貸している実行権ではなかった (別の実行インスタンスのもの、返し済みのもの)。
    /// 何も変えておらず、実行器も動かしていない。
    Rejected {
        /// 受け取らなかった実行権。貸した `Kernel` へ返す。
        permit: RunPermit,
        /// 実行のために取り出していた文脈 ([`finish_run`] が拒否したとき)。
        loaded: Option<Loaded<C>>,
        /// 理由。
        error: Error,
    },
    /// 実行権はこの `Kernel` へ返した (スレッドはサービス中)。または、受理した後に内部の障害を見つけた。
    Failed(Error),
}

impl<C> NotRun<C> {
    /// 理由。
    #[must_use]
    pub const fn error(&self) -> Error {
        match self {
            NotRun::Rejected { error, .. } | NotRun::Failed(error) => *error,
        }
    }

    /// 受け取らなかった実行権を取り戻す。`Kernel` へ返し済みなら `None`。
    #[must_use]
    pub fn into_permit(self) -> Option<RunPermit> {
        match self {
            NotRun::Rejected { permit, .. } => Some(permit),
            NotRun::Failed(_) => None,
        }
    }
}

/// worker の一回ぶん: 実行権を借り、文脈を載せ、実行し、文脈を降ろし、実行権を返す。
/// 貸せるスレッドが無ければ `None`。
pub fn run_once<E: Executor>(
    kernel: &mut Kernel,
    worker: WorkerId,
    executor: &mut E,
    contexts: &mut Contexts<E::Context>,
) -> Result<Option<Ran>, NotRun<E::Context>> {
    match kernel.grant(worker).map_err(NotRun::Failed)? {
        Some(permit) => run_permit(kernel, permit, executor, contexts).map(Some),
        None => Ok(None),
    }
}

/// 借りた実行権で一回実行する。スレッドを名指しして借りた実行権 ([`Kernel::grant_thread`]) にも使う。
///
/// `kernel` が貸している実行権でなければ、実行器を動かさずに、実行権をそのまま返す
/// ([`NotRun::Rejected`])。それ以外の失敗を返すときは、実行権を必ず `kernel` へ返している: 文脈が
/// 無い、または実行器が状況と合わない理由を返したときは、スレッドをサービス中にして ([`Stop::Trap`])、
/// その失敗を返す。スレッドが終了したら文脈を取り除く。
pub fn run_permit<E: Executor>(
    kernel: &mut Kernel,
    permit: RunPermit,
    executor: &mut E,
    contexts: &mut Contexts<E::Context>,
) -> Result<Ran, NotRun<E::Context>> {
    let (permit, mut loaded) = start_run(kernel, permit, contexts)?;
    let stopped = executor.run(&permit, loaded.context());
    finish_run(kernel, permit, loaded, stopped, contexts)
}

/// 実行の前半: 実行権が `kernel` の貸しているものであることを確かめ、合う文脈を置き場から取り出す。
///
/// [`run_permit`] を二つに分けたもの。ゲストを実行している間は `Kernel` を借りないので、
/// 複数の worker が一つの `Kernel` を順番に使いながら、ゲストは並行に実行できる。
/// 文脈が無ければ、実行権を返してから ([`Stop::Trap`]) その失敗を返す。
pub fn start_run<C>(
    kernel: &mut Kernel,
    permit: RunPermit,
    contexts: &mut Contexts<C>,
) -> Result<(RunPermit, Loaded<C>), NotRun<C>> {
    if let Err(error) = kernel.lent(&permit) {
        return Err(NotRun::Rejected { permit, loaded: None, error });
    }
    match contexts.load(&permit) {
        Ok(loaded) => Ok((permit, loaded)),
        Err(error) => Err(hand_back(kernel, permit, Budget::new(0), error)),
    }
}

/// 実行の後半: 文脈を置き場へ戻し、止まった理由を添えて実行権を返す。
///
/// 失敗を返すときの実行権の行き先は [`run_permit`] と同じ。
pub fn finish_run<C>(
    kernel: &mut Kernel,
    permit: RunPermit,
    loaded: Loaded<C>,
    stopped: Stopped,
    contexts: &mut Contexts<C>,
) -> Result<Ran, NotRun<C>> {
    if let Err(error) = kernel.lent(&permit) {
        return Err(NotRun::Rejected { permit, loaded: Some(loaded), error });
    }
    let thread = permit.thread();
    if let Err(error) = contexts.save(loaded) {
        return Err(hand_back(kernel, permit, stopped.consumed, error));
    }
    let fate = match kernel.relinquish(permit, stopped.stop, stopped.consumed) {
        Ok(fate) => fate,
        Err(NotRelinquished::Rejected(permit, error)) => return Err(hand_back(kernel, permit, stopped.consumed, error)),
        Err(NotRelinquished::Failed(error)) => return Err(NotRun::Failed(error)),
    };
    match fate {
        ThreadFate::Exited(_) => {
            contexts.remove(thread).map_err(NotRun::Failed)?;
        }
        ThreadFate::Ready | ThreadFate::Servicing => {}
    }
    Ok(Ran { thread, stopped, fate })
}

/// 続きを実行できなくなった実行権を返し、スレッドをサービス中にして、元の失敗 `why` を返す。
/// ここでも受け取られなかった実行権は、失わずに呼び出した側へ返す。
fn hand_back<C>(kernel: &mut Kernel, permit: RunPermit, consumed: Budget, why: Error) -> NotRun<C> {
    match kernel.relinquish(permit, Stop::Trap, consumed) {
        Ok(_) => NotRun::Failed(why),
        Err(NotRelinquished::Rejected(permit, error)) => NotRun::Rejected { permit, loaded: None, error },
        Err(NotRelinquished::Failed(error)) => NotRun::Failed(error),
    }
}
