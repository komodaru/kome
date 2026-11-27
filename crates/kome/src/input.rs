//! [`Kernel`] に入る情報を一つの型で表したもの。
//!
//! 状態を変える操作は、すべて [`Input`] の一つの値として表せる。[`Kernel::apply`] は
//! それを対応するメソッドへ渡すだけである。記録・再生と決定的シミュレーションは、
//! 初期状態と `Input` の列から同じ状態と同じ [`crate::Effect`] の列を得る。
//!
//! 参照だけの問い合わせ (`translate`、`next_deadline` など) は状態を変えないので、
//! ここには無い。

use crate::addr::{AddrWidth, GuestRange, PageSize, Prot};
use crate::effect::Completion;
use crate::error::Error;
use crate::id::{BackingId, IoId, ObjectId, ProcessId, ThreadId, TimerId};
use crate::io::{CancelReply, IoLinks, IoOutcome, IoSettled};
use crate::kernel::Kernel;
use crate::mem::{MapGen, MapSource};
use crate::object::{SignalOp, SignalSpec};
use crate::process::ExitCode;
use crate::sched::{Budget, RunPermit, Stop, ThreadFate, WorkerId};
use crate::thread::Start;
use crate::time::{Deadline, Instant, Period};
use crate::wait::{Interruptible, WaitMode, WaitStart};

/// 状態を変える一つの操作。各項目は同名の [`Kernel`] のメソッドに対応し、引数の順も同じである。
#[derive(Debug, PartialEq, Eq)]
pub enum Input<'a> {
    /// [`Kernel::create_process`]
    CreateProcess(AddrWidth, PageSize),
    /// [`Kernel::exit_process`]
    ExitProcess(ProcessId, ExitCode),
    /// [`Kernel::create_thread`]
    CreateThread(ProcessId, Start),
    /// [`Kernel::continue_thread`]
    ContinueThread(ThreadId),
    /// [`Kernel::suspend_thread`]
    SuspendThread(ThreadId),
    /// [`Kernel::unsuspend_thread`]
    UnsuspendThread(ThreadId),
    /// [`Kernel::terminate_thread`]
    TerminateThread(ThreadId, ExitCode),
    /// [`Kernel::grant`]
    Grant(WorkerId),
    /// [`Kernel::relinquish`]
    Relinquish(RunPermit, Stop, Budget),
    /// [`Kernel::create_object`]
    CreateObject(ProcessId, SignalSpec),
    /// [`Kernel::retain_object`]
    RetainObject(ProcessId, ObjectId),
    /// [`Kernel::release_object`]
    ReleaseObject(ProcessId, ObjectId),
    /// [`Kernel::signal`]
    Signal(ObjectId, SignalOp),
    /// [`Kernel::wait`]
    Wait(ThreadId, &'a [ObjectId], WaitMode, Deadline, Interruptible),
    /// [`Kernel::interrupt_wait`]
    InterruptWait(ThreadId),
    /// [`Kernel::create_timer`]
    CreateTimer(ProcessId, ObjectId),
    /// [`Kernel::arm_timer`]
    ArmTimer(TimerId, Instant, Option<Period>),
    /// [`Kernel::disarm_timer`]
    DisarmTimer(TimerId),
    /// [`Kernel::drop_timer`]
    DropTimer(TimerId),
    /// [`Kernel::advance`]
    Advance(Instant),
    /// [`Kernel::prepare_io`]
    PrepareIo(ProcessId, IoLinks),
    /// [`Kernel::issue_io`]
    IssueIo(IoId),
    /// [`Kernel::cancel_io`]
    CancelIo(IoId),
    /// [`Kernel::complete`]
    Complete(Completion),
    /// [`Kernel::publish_io`]
    PublishIo(IoId),
    /// [`Kernel::release_io`]
    ReleaseIo(IoId),
    /// [`Kernel::create_backing`]
    CreateBacking(ProcessId, u64),
    /// [`Kernel::retain_backing`]
    RetainBacking(ProcessId, BackingId),
    /// [`Kernel::release_backing`]
    ReleaseBacking(ProcessId, BackingId),
    /// [`Kernel::map`]
    Map(ProcessId, GuestRange, MapSource, Prot),
    /// [`Kernel::unmap`]
    Unmap(ProcessId, GuestRange),
    /// [`Kernel::protect`]
    Protect(ProcessId, GuestRange, Prot),
}

/// 操作への返答。
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
    /// 返す値の無い操作が済んだ。
    Done,
    /// プロセスと、その終了通知の資源を作った。
    Process(ProcessId, ObjectId),
    /// スレッドと、その終了通知の資源を作った。
    Thread(ThreadId, ObjectId),
    /// 待機可能な資源を作った。
    Object(ObjectId),
    /// 実体を登録した。
    Backing(BackingId),
    /// タイマを作った。
    Timer(TimerId),
    /// I/O 要求を準備した。
    Io(IoId),
    /// 実行権を貸した。実行可能なスレッドが無ければ `None`。
    Permit(Option<RunPermit>),
    /// 実行権を受け取った後のスレッド。
    Fate(ThreadFate),
    /// 変更前の数 (信号の数、停止要求の数)、処理した期限の数、外したバイト数。
    Count(u64),
    /// タイマが設定されていたか。
    WasArmed(bool),
    /// 待機の始まり方。
    Wait(WaitStart),
    /// 取消の受け付け方。
    Cancel(CancelReply),
    /// 確定した終端結果。
    Settled(IoSettled),
    /// 公開した終端結果。
    Outcome(IoOutcome),
    /// 変更後のアドレス空間の世代。
    Generation(MapGen),
}

impl Kernel {
    /// 一つの操作を適用する。
    pub fn apply(&mut self, input: Input<'_>) -> Result<Reply, Error> {
        Ok(match input {
            Input::CreateProcess(width, page) => {
                let (process, exit) = self.create_process(width, page)?;
                Reply::Process(process, exit)
            }
            Input::ExitProcess(process, code) => self.exit_process(process, code).map(|()| Reply::Done)?,
            Input::CreateThread(process, start) => {
                let (thread, exit) = self.create_thread(process, start)?;
                Reply::Thread(thread, exit)
            }
            Input::ContinueThread(thread) => self.continue_thread(thread).map(|()| Reply::Done)?,
            Input::SuspendThread(thread) => Reply::Count(u64::from(self.suspend_thread(thread)?)),
            Input::UnsuspendThread(thread) => Reply::Count(u64::from(self.unsuspend_thread(thread)?)),
            Input::TerminateThread(thread, code) => self.terminate_thread(thread, code).map(|()| Reply::Done)?,
            Input::Grant(worker) => Reply::Permit(self.grant(worker)?),
            Input::Relinquish(permit, stop, consumed) => Reply::Fate(self.relinquish(permit, stop, consumed)?),
            Input::CreateObject(process, spec) => Reply::Object(self.create_object(process, spec)?),
            Input::RetainObject(process, object) => self.retain_object(process, object).map(|()| Reply::Done)?,
            Input::ReleaseObject(process, object) => self.release_object(process, object).map(|()| Reply::Done)?,
            Input::Signal(object, op) => Reply::Count(self.signal(object, op)?),
            Input::Wait(thread, targets, mode, deadline, interruptible) => {
                Reply::Wait(self.wait(thread, targets, mode, deadline, interruptible)?)
            }
            Input::InterruptWait(thread) => self.interrupt_wait(thread).map(|()| Reply::Done)?,
            Input::CreateTimer(process, object) => Reply::Timer(self.create_timer(process, object)?),
            Input::ArmTimer(timer, at, period) => self.arm_timer(timer, at, period).map(|()| Reply::Done)?,
            Input::DisarmTimer(timer) => Reply::WasArmed(self.disarm_timer(timer)?),
            Input::DropTimer(timer) => self.drop_timer(timer).map(|()| Reply::Done)?,
            Input::Advance(to) => Reply::Count(u64::from(self.advance(to)?)),
            Input::PrepareIo(process, links) => Reply::Io(self.prepare_io(process, links)?),
            Input::IssueIo(io) => self.issue_io(io).map(|()| Reply::Done)?,
            Input::CancelIo(io) => Reply::Cancel(self.cancel_io(io)?),
            Input::Complete(completion) => Reply::Settled(self.complete(completion)?),
            Input::PublishIo(io) => Reply::Outcome(self.publish_io(io)?),
            Input::ReleaseIo(io) => self.release_io(io).map(|()| Reply::Done)?,
            Input::CreateBacking(process, len) => Reply::Backing(self.create_backing(process, len)?),
            Input::RetainBacking(process, backing) => {
                self.retain_backing(process, backing).map(|()| Reply::Done)?
            }
            Input::ReleaseBacking(process, backing) => {
                self.release_backing(process, backing).map(|()| Reply::Done)?
            }
            Input::Map(process, range, source, prot) => Reply::Generation(self.map(process, range, source, prot)?),
            Input::Unmap(process, range) => Reply::Count(self.unmap(process, range)?),
            Input::Protect(process, range, prot) => Reply::Generation(self.protect(process, range, prot)?),
        })
    }
}
