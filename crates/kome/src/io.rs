//! 非同期 I/O の要求: 進行状態、取消要求、終端の確定。
//!
//! kome が持つのは要求の ID と進行状態だけである。内容 (何を読むか、バッファ) は
//! 実行統括とサービスが ID に対応づけて持つ。`OVERLAPPED` や完了ポートの意味、API ごとの
//! 出力は Personality が持つ。
//!
//! 進行状態は五つある: 準備中、発行済み、取消要求済み、終端確定済み、公開済み。
//!
//! - 取消は要求であり、完了ではない。取消を要求した後に正常完了することがある。
//!   終端結果を観測するまで、要求 (とサービス側のバッファ) の寿命は続く。
//! - 終端結果は一度だけ確定する (台帳 IO-1)。二度目の完了通知は拒否する。
//! - 完了通知を受けても、通知先の資源はまだ変えない。Personality が出力をゲストへ
//!   書いた後で [`Kernel::publish_io`] を呼ぶと、通知先の数を増やし、待機を解除する。
//!   起こされたスレッドが、まだ更新されていない結果を読むことは無い。
//!
//! ここで言う「一度だけ」は、kome の中で終端結果を一度だけ確定することである。
//! 外部への書き込みが一度だけ実行されることは保証しない。

use crate::effect::{Completion, CompletionKind, Effect};
use crate::error::{Error, Invariant, Refusal};
use crate::id::{IoId, ObjectId, ProcessId};
use crate::kernel::Kernel;
use crate::process::Life;
use crate::store::plain_try_clone;

/// ホストが報告する失敗の番号。意味はホスト実装とサービスの間で決める。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostError(u32);

impl HostError {
    /// 数値から作る。
    #[must_use]
    pub const fn new(value: u32) -> HostError {
        HostError(value)
    }

    /// 失敗の番号。
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// I/O の終端結果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoOutcome {
    /// 完了した。要求より少ない量で完了することがある。
    Done {
        /// 転送したバイト数。
        transferred: u64,
    },
    /// 失敗した。
    Failed {
        /// ホストの失敗の番号。
        error: HostError,
        /// 失敗までに転送したバイト数。
        transferred: u64,
    },
    /// 取り消された。
    Cancelled {
        /// 取消までに転送したバイト数。
        transferred: u64,
    },
}

/// I/O の進行状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoPhase {
    /// 準備中。まだホストへ出していない。
    Prepared,
    /// 発行済み。ホストの応答を待っている。
    Issued,
    /// 取消要求済み。ホストの応答 (正常完了のこともある) を待っている。
    CancelRequested,
    /// 終端結果が確定した。まだ通知していない。
    Settled,
    /// 通知先へ公開した。
    Published,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IoState {
    Prepared,
    Issued,
    CancelRequested,
    Settled(IoOutcome),
    Published(IoOutcome),
}

impl IoState {
    pub(crate) const fn phase(self) -> IoPhase {
        match self {
            IoState::Prepared => IoPhase::Prepared,
            IoState::Issued => IoPhase::Issued,
            IoState::CancelRequested => IoPhase::CancelRequested,
            IoState::Settled(_) => IoPhase::Settled,
            IoState::Published(_) => IoPhase::Published,
        }
    }

    /// ホストの応答 (完了通知) を待っているか。
    pub(crate) const fn awaits_host(self) -> bool {
        match self {
            IoState::Issued | IoState::CancelRequested => true,
            IoState::Prepared | IoState::Settled(_) | IoState::Published(_) => false,
        }
    }

    /// この状態で予約している `Effect` の席 (開始と取消)。
    pub(crate) const fn promises(self) -> u32 {
        match self {
            IoState::Prepared => 2,
            IoState::Issued => 1,
            IoState::CancelRequested | IoState::Settled(_) | IoState::Published(_) => 0,
        }
    }
}

/// 進行状態を変える出来事。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IoEvent {
    Issue,
    Cancel,
    Complete(IoOutcome),
    Publish,
}

/// 遷移に伴って出す `Effect` の種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IoEmit {
    Nothing,
    Submit,
    Cancel,
}

/// I/O の遷移規則。状態と出来事だけで決まる。`tla/IoLifecycle.tla` の `Next` と対応する。
pub(crate) const fn io_step(state: IoState, event: IoEvent) -> Result<(IoState, IoEmit), Refusal> {
    match (state, event) {
        (IoState::Prepared, IoEvent::Issue) => Ok((IoState::Issued, IoEmit::Submit)),
        (IoState::Prepared, IoEvent::Cancel) => {
            Ok((IoState::Settled(IoOutcome::Cancelled { transferred: 0 }), IoEmit::Nothing))
        }
        (IoState::Prepared, IoEvent::Complete(_)) => Err(Refusal::IoNotIssued),
        (IoState::Issued, IoEvent::Cancel) => Ok((IoState::CancelRequested, IoEmit::Cancel)),
        (IoState::CancelRequested, IoEvent::Cancel) => Ok((IoState::CancelRequested, IoEmit::Nothing)),
        (IoState::Issued | IoState::CancelRequested, IoEvent::Issue) => Err(Refusal::IoAlreadyIssued),
        (IoState::Issued | IoState::CancelRequested, IoEvent::Complete(outcome)) => {
            Ok((IoState::Settled(outcome), IoEmit::Nothing))
        }
        (IoState::Settled(outcome), IoEvent::Publish) => Ok((IoState::Published(outcome), IoEmit::Nothing)),
        (IoState::Prepared | IoState::Issued | IoState::CancelRequested | IoState::Published(_), IoEvent::Publish) => {
            Err(Refusal::IoNotPublishable)
        }
        (IoState::Settled(_) | IoState::Published(_), IoEvent::Issue | IoEvent::Cancel | IoEvent::Complete(_)) => {
            Err(Refusal::IoSettled)
        }
    }
}

/// [`Kernel::cancel_io`] の返答。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelReply {
    /// 発行前だったので、取消として終端を確定した。ホストへは何も出していない。
    Withdrawn,
    /// 取消を要求した。終端結果はホストの応答で確定する。
    Requested,
    /// 既に取消を要求してある。
    AlreadyRequested,
}

/// [`Kernel::complete`] の返答。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoSettled {
    /// 終端結果を確定し、要求を保持している。出力を書いてから公開する。
    Held(IoOutcome),
    /// 持ち主のプロセスが終了済みだったので、確定と同時に要求を回収した。
    /// 結果を受け取るゲストはいない。サービスは対応するバッファを捨てる。
    Orphaned(IoOutcome),
}

/// I/O 要求が使用中として固定する資源。要求が回収されるまで破棄されない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoLinks {
    target: Option<ObjectId>,
    notify: Option<ObjectId>,
}

impl IoLinks {
    /// どの資源も固定しない。
    pub const NONE: IoLinks = IoLinks { target: None, notify: None };

    /// 要求の対象 (ファイルなど) を表す資源を固定する。
    #[must_use]
    pub const fn with_target(mut self, target: ObjectId) -> IoLinks {
        self.target = Some(target);
        self
    }

    /// [`Kernel::publish_io`] で数を増やす通知先を固定する。
    #[must_use]
    pub const fn with_notify(mut self, notify: ObjectId) -> IoLinks {
        self.notify = Some(notify);
        self
    }
}

plain_try_clone!(Io);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Io {
    pub(crate) process: ProcessId,
    /// 要求の対象 (ファイルなど) を表す資源。未完了の間、破棄させない。
    pub(crate) target: Option<ObjectId>,
    /// 公開のときに数を増やす資源。
    pub(crate) notify: Option<ObjectId>,
    pub(crate) state: IoState,
}

impl Kernel {
    /// I/O 要求を準備する。`links` の資源は要求が回収されるまで破棄されない。
    pub fn prepare_io(&mut self, process: ProcessId, links: IoLinks) -> Result<IoId, Error> {
        let IoLinks { target, notify } = links;
        let owner = self.processes.find(process, &self.lineage)?;
        if owner.life != Life::Alive {
            return Err(Error::Refused(Refusal::ProcessExiting));
        }
        let ios = owner.ios.checked_add(1).ok_or(Error::Exhausted(crate::error::Resource::Ios))?;
        match (target, notify) {
            (Some(target), Some(notify)) if target == notify => self.can_notify(target, 2)?,
            (target, notify) => {
                if let Some(target) = target {
                    self.can_pin(target, 1)?;
                }
                if let Some(notify) = notify {
                    self.can_notify(notify, 1)?;
                }
            }
        }
        self.ios.reserve()?;
        // 開始と取消の席。
        self.effects.promise(2)?;
        let io = self.ios.insert(Io { process, target, notify, state: IoState::Prepared })?;
        for object in [target, notify].into_iter().flatten() {
            self.pin(object)?;
        }
        self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?.ios = ios;
        Ok(io)
    }

    /// 準備した要求をホストへ出す。
    pub fn issue_io(&mut self, io: IoId) -> Result<(), Error> {
        self.io_event(io, IoEvent::Issue).map(|_| ())
    }

    /// 取消を要求する。発行前なら取消として終端を確定する。
    pub fn cancel_io(&mut self, io: IoId) -> Result<CancelReply, Error> {
        let reply = match self.ios.find(io, &self.lineage)?.state {
            IoState::Prepared => CancelReply::Withdrawn,
            IoState::Issued => CancelReply::Requested,
            IoState::CancelRequested => CancelReply::AlreadyRequested,
            IoState::Settled(_) | IoState::Published(_) => return Err(Error::Refused(Refusal::IoSettled)),
        };
        self.io_event(io, IoEvent::Cancel)?;
        Ok(reply)
    }

    /// ホストからの完了通知を受け、終端結果を一度だけ確定する。
    ///
    /// 回収済みの要求への遅い通知は [`Error::Stale`]、二度目の通知は
    /// `Refused(IoSettled)`、別インスタンスへの通知は [`Error::Foreign`] になり、
    /// いずれも状態を変えない。
    pub fn complete(&mut self, completion: Completion) -> Result<IoSettled, Error> {
        if completion.instance() != self.lineage.instance() {
            return Err(Error::Foreign);
        }
        match completion.kind() {
            CompletionKind::Io { io, outcome } => {
                let process = self.ios.find(io, &self.lineage)?.process;
                self.io_event(io, IoEvent::Complete(outcome))?;
                let owner = self.processes.get(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
                match owner.life {
                    Life::Alive | Life::Exiting(_) => Ok(IoSettled::Held(outcome)),
                    Life::Draining(_) => {
                        self.remove_io(io)?;
                        self.reclaim_process_if_drained(process)?;
                        Ok(IoSettled::Orphaned(outcome))
                    }
                }
            }
        }
    }

    /// 確定した終端結果を通知先へ公開する。通知先の数を増やし、待機を解除する。
    ///
    /// Personality は、ゲストから見える出力をすべて書いた後で呼ぶ。
    pub fn publish_io(&mut self, io: IoId) -> Result<IoOutcome, Error> {
        let IoState::Published(outcome) = self.io_event(io, IoEvent::Publish)? else {
            return Err(Error::Broken(Invariant::IoOnce));
        };
        let notify = self.ios.get(io).ok_or(Error::Broken(Invariant::IoOnce))?.notify;
        if let Some(notify) = notify {
            self.raise(notify)?;
        }
        Ok(outcome)
    }

    /// 要求を回収する。ホストの応答を待っている間は拒否する。
    pub fn release_io(&mut self, io: IoId) -> Result<(), Error> {
        let entry = self.ios.find(io, &self.lineage)?;
        match entry.state {
            IoState::Issued | IoState::CancelRequested => Err(Error::Refused(Refusal::IoInFlight)),
            IoState::Prepared | IoState::Settled(_) | IoState::Published(_) => self.remove_io(io),
        }
    }

    /// I/O の進行状態。
    pub fn io_phase(&self, io: IoId) -> Result<IoPhase, Error> {
        Ok(self.ios.find(io, &self.lineage)?.state.phase())
    }

    /// 確定済みの終端結果。未確定なら `None`。
    pub fn io_outcome(&self, io: IoId) -> Result<Option<IoOutcome>, Error> {
        Ok(match self.ios.find(io, &self.lineage)?.state {
            IoState::Settled(outcome) | IoState::Published(outcome) => Some(outcome),
            IoState::Prepared | IoState::Issued | IoState::CancelRequested => None,
        })
    }

    fn io_event(&mut self, io: IoId, event: IoEvent) -> Result<IoState, Error> {
        let entry = self.ios.find_mut(io, &self.lineage)?;
        let before = entry.state;
        let (after, emit) = io_step(before, event).map_err(Error::Refused)?;
        entry.state = after;
        match emit {
            IoEmit::Nothing => {}
            IoEmit::Submit => self.effects.emit(Effect::IoSubmit { io })?,
            IoEmit::Cancel => self.effects.emit(Effect::IoCancel { io })?,
        }
        let emitted = match emit {
            IoEmit::Nothing => 0,
            IoEmit::Submit | IoEmit::Cancel => 1,
        };
        // 遷移で不要になった席を返す (例: 取消を要求しないまま終端した)。
        let unused = before
            .promises()
            .checked_sub(after.promises())
            .and_then(|released| released.checked_sub(emitted))
            .ok_or(Error::Broken(Invariant::EffectsKept))?;
        self.effects.forgo(unused)?;
        Ok(after)
    }

    /// 要求の記録を消し、席と固定を返す。確保しない。
    fn remove_io(&mut self, io: IoId) -> Result<(), Error> {
        let entry = self.ios.remove(io).ok_or(Error::Broken(Invariant::IoOnce))?;
        self.effects.forgo(entry.state.promises())?;
        let owner = self.processes.get_mut(entry.process).ok_or(Error::Broken(Invariant::ProcessTree))?;
        owner.ios = owner.ios.checked_sub(1).ok_or(Error::Broken(Invariant::ProcessTree))?;
        for object in [entry.target, entry.notify].into_iter().flatten() {
            self.unpin(object)?;
        }
        Ok(())
    }

    /// 終了するプロセスの I/O を手放す。ホストの応答を待つものは取消を要求して残す。
    pub(crate) fn abandon_ios_of(&mut self, process: ProcessId) -> Result<(), Error> {
        let mut slot = 0u32;
        loop {
            let found = self
                .ios
                .iter_from(slot)
                .find(|(_, io)| io.process == process)
                .map(|(id, io)| (id, io.state));
            let Some((io, state)) = found else { return Ok(()) };
            slot = io.slot().saturating_add(1);
            match state {
                IoState::Prepared | IoState::Settled(_) | IoState::Published(_) => self.remove_io(io)?,
                IoState::Issued => {
                    self.io_event(io, IoEvent::Cancel).map_err(|_| Error::Broken(Invariant::IoOnce))?;
                }
                IoState::CancelRequested => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DONE: IoOutcome = IoOutcome::Done { transferred: 7 };
    const STATES: [IoState; 5] = [
        IoState::Prepared,
        IoState::Issued,
        IoState::CancelRequested,
        IoState::Settled(DONE),
        IoState::Published(DONE),
    ];
    const EVENTS: [IoEvent; 4] = [IoEvent::Issue, IoEvent::Cancel, IoEvent::Complete(DONE), IoEvent::Publish];

    #[test]
    fn a_settled_outcome_never_changes() {
        for state in [IoState::Settled(DONE), IoState::Published(DONE)] {
            for event in EVENTS {
                if let Ok((after, _)) = io_step(state, event) {
                    assert_eq!(after, IoState::Published(DONE), "{state:?} + {event:?}");
                }
            }
        }
    }

    #[test]
    fn promises_cover_every_emission() {
        for state in STATES {
            for event in EVENTS {
                if let Ok((after, emit)) = io_step(state, event) {
                    let emitted = u32::from(emit != IoEmit::Nothing);
                    assert!(state.promises() >= after.promises() + emitted, "{state:?} + {event:?}");
                }
            }
        }
    }

    #[test]
    fn completion_is_accepted_only_while_in_flight() {
        for state in STATES {
            let accepted = io_step(state, IoEvent::Complete(DONE)).is_ok();
            assert_eq!(accepted, matches!(state, IoState::Issued | IoState::CancelRequested), "{state:?}");
        }
    }

    const fn state_name(state: IoState) -> &'static str {
        match state {
            IoState::Prepared => "Prepared",
            IoState::Issued => "Issued",
            IoState::CancelRequested => "CancelRequested",
            IoState::Settled(_) => "Settled",
            IoState::Published(_) => "Published",
        }
    }

    const fn event_name(event: IoEvent) -> &'static str {
        match event {
            IoEvent::Issue => "Issue",
            IoEvent::Cancel => "Cancel",
            IoEvent::Complete(_) => "Complete",
            IoEvent::Publish => "Publish",
        }
    }

    const fn emit_name(emit: IoEmit) -> &'static str {
        match emit {
            IoEmit::Nothing => "Nothing",
            IoEmit::Submit => "Submit",
            IoEmit::Cancel => "Cancel",
        }
    }

    /// `io_step` の受理する遷移を TLA+ の表として書き出す。
    fn tla_table() -> String {
        let quoted = |names: Vec<&str>| names.iter().map(|n| format!("\"{n}\"")).collect::<Vec<_>>().join(", ");
        let mut rows = Vec::new();
        for state in STATES {
            for event in EVENTS {
                if let Ok((after, emit)) = io_step(state, event) {
                    rows.push(format!(
                        "    (<<\"{}\", \"{}\">> :> <<\"{}\", \"{}\">>)",
                        state_name(state),
                        event_name(event),
                        state_name(after),
                        emit_name(emit)
                    ));
                }
            }
        }
        format!(
            "------------------------------ MODULE IoTable ------------------------------\n\
             (* crates/kome/src/io.rs の io_step から生成した。手で書き換えない。\n\
             \x20  書き直す: KOME_UPDATE_GOLDEN=1 cargo test -p kome the_tla_table_is_io_step *)\n\
             EXTENDS TLC\n\n\
             IoStates == {{{}}}\n\
             IoEvents == {{{}}}\n\n\
             \\* 受理する (状態, 出来事) から (次の状態, 出す Effect) への表。載っていない組は拒否する。\n\
             IoStep ==\n{}\n\
             =============================================================================\n",
            quoted(STATES.map(state_name).to_vec()),
            quoted(EVENTS.map(event_name).to_vec()),
            rows.join(" @@\n")
        )
    }

    /// TLA+ のモデル (tla/IoLifecycle.tla) が使う表は、この実装そのものである。
    #[test]
    fn the_tla_table_is_io_step() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tla/IoTable.tla");
        let table = tla_table();
        if std::env::var("KOME_UPDATE_GOLDEN").is_ok_and(|v| !v.is_empty()) {
            std::fs::write(path, &table).unwrap();
        }
        let on_disk = std::fs::read_to_string(path).unwrap();
        assert_eq!(on_disk, table, "tla/IoTable.tla が io_step と違う。KOME_UPDATE_GOLDEN=1 で書き直す");
    }

    /// 内容だけが違う状態・出来事は、遷移の形を変えない (表が内容を落としてよい根拠)。
    #[test]
    fn the_step_shape_ignores_payloads() {
        let other = IoOutcome::Failed { error: HostError::new(5), transferred: 1 };
        for (state, twin) in [(IoState::Settled(DONE), IoState::Settled(other)), (IoState::Published(DONE), IoState::Published(other))] {
            for event in EVENTS {
                let shape = |r: Result<(IoState, IoEmit), Refusal>| r.map(|(s, e)| (state_name(s), emit_name(e)));
                assert_eq!(shape(io_step(state, event)), shape(io_step(twin, event)));
            }
        }
        for state in STATES {
            let shape = |r: Result<(IoState, IoEmit), Refusal>| r.map(|(s, e)| (state_name(s), emit_name(e)));
            assert_eq!(shape(io_step(state, IoEvent::Complete(DONE))), shape(io_step(state, IoEvent::Complete(other))));
        }
    }

    #[test]
    fn cancel_after_issue_is_a_request_not_an_outcome() {
        let (state, emit) = io_step(IoState::Issued, IoEvent::Cancel).unwrap();
        assert_eq!((state, emit), (IoState::CancelRequested, IoEmit::Cancel));
        // 取消を要求した後でも正常完了を受理する。
        assert_eq!(io_step(state, IoEvent::Complete(DONE)).unwrap().0, IoState::Settled(DONE));
    }
}
