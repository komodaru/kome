//! プロセス: 所属するスレッド、参照、アドレス空間、寿命、終了処理。
//!
//! 実行形式、DLL、起動環境は Personality が持つ。kome のプロセスは資源の所有者である。
//!
//! 寿命は三段階ある。`Alive` から、終了要求または最後のスレッドの終了で `Exiting` に
//! 入る。実行中のスレッドが実行権を返し終えると、タイマ・I/O・参照・アドレス空間を
//! 手放して `Draining` になり、終了を通知する。ホストの応答を待つ I/O がすべて終端
//! したとき、プロセスの記録を回収する。

use crate::addr::{AddrWidth, PageSize};
use crate::effect::Effect;
use crate::error::{Error, Invariant, Refusal, Resource};
use crate::id::{Id, Kind, ObjectId, ProcessId, ThreadId, kind};
use crate::ipc::End;
use crate::kernel::Kernel;
use crate::mem::Space;
use crate::object::{Object, SignalMode, SignalSpec};
use crate::store::{List, TryClone};
use crate::thread::Run;

/// 終了コード。意味は Personality が決める。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExitCode(u64);

impl ExitCode {
    /// 数値から作る。
    #[must_use]
    pub const fn new(value: u64) -> ExitCode {
        ExitCode(value)
    }

    /// 終了コードの数値。
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Life {
    Alive,
    /// 終了が決まった。実行中のスレッドが実行権を返すのを待っている。
    Exiting(ExitCode),
    /// スレッドも参照も無い。ホストの応答を待つ I/O だけが残っている。
    Draining(ExitCode),
}

/// プロセスの寿命の段階。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessPhase {
    /// 生きている。
    Alive,
    /// 終了が決まり、実行中のスレッドが止まるのを待っている。
    Exiting,
    /// 終了を通知済みで、未完了 I/O の終端を待っている。
    Draining,
}

/// プロセスが作られたときに返すもの: プロセスと、終了時に数が 1 になる資源。
pub type ProcessCreated = (ProcessId, ObjectId);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Held<K: Kind> {
    id: Id<K>,
    count: u32,
}

impl<K: Kind> TryClone for Held<K> {
    fn try_clone(&self) -> Result<Held<K>, Error> {
        Ok(Held { id: self.id, count: self.count })
    }
}

impl<K: Kind> TryClone for Holdings<K> {
    fn try_clone(&self) -> Result<Holdings<K>, Error> {
        Ok(Holdings { list: self.list.try_clone()? })
    }
}

/// 一つのプロセスが持つ参照。ID の昇順。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Holdings<K: Kind> {
    list: List<Held<K>>,
}

impl<K: Kind> Holdings<K> {
    pub(crate) const fn new() -> Holdings<K> {
        Holdings { list: List::new() }
    }

    fn find(&self, id: Id<K>) -> Result<usize, usize> {
        self.list.as_slice().binary_search_by(|held| held.id.cmp(&id))
    }

    /// 新しい種類の参照を一つ追加できることを保証する。
    pub(crate) fn room_for_new(&mut self, limit: u32) -> Result<(), Error> {
        if u64::try_from(self.list.len()).unwrap_or(u64::MAX) >= u64::from(limit) {
            return Err(Error::Exhausted(Resource::Holdings));
        }
        self.list.reserve(1)
    }

    /// 参照を一つ増やす。失敗したら何も変えない。
    pub(crate) fn add(&mut self, id: Id<K>, limit: u32) -> Result<(), Error> {
        match self.find(id) {
            Ok(at) => {
                let held = self.list.get_mut(at).ok_or(Error::Broken(Invariant::ObjectRefs))?;
                held.count = held.count.checked_add(1).ok_or(Error::Refused(Refusal::HoldOverflow))?;
                Ok(())
            }
            Err(at) => {
                self.room_for_new(limit)?;
                self.list.insert(at, Held { id, count: 1 }, Invariant::ObjectRefs)
            }
        }
    }

    /// 参照を一つ減らす。持っていなければ拒否する。
    pub(crate) fn sub(&mut self, id: Id<K>) -> Result<(), Error> {
        let at = self.find(id).map_err(|_| Error::Refused(Refusal::NotHeld))?;
        let held = self.list.get_mut(at).ok_or(Error::Broken(Invariant::ObjectRefs))?;
        held.count = held.count.checked_sub(1).ok_or(Error::Broken(Invariant::ObjectRefs))?;
        if held.count == 0 {
            self.list.remove(at).ok_or(Error::Broken(Invariant::ObjectRefs))?;
        }
        Ok(())
    }

    /// 参照を一つ取り出して記録から消す。終了処理で使う。
    pub(crate) fn take_one(&mut self) -> Option<Id<K>> {
        let at = self.list.len().checked_sub(1)?;
        let held = self.list.get_mut(at)?;
        let id = held.id;
        held.count = held.count.saturating_sub(1);
        if held.count == 0 {
            // 外せなければ同じ参照を繰り返し取り出すことになるので、ここで止める。
            self.list.remove(at)?;
        }
        Some(id)
    }

    /// 先頭の参照の数を書き換える。検査が違反を検出できることを確かめる試験のための入口。
    #[cfg(test)]
    pub(crate) fn corrupt_first_count(&mut self, count: u32) {
        if let Some(held) = self.list.get_mut(0) {
            held.count = count;
        }
    }

    pub(crate) fn count_of(&self, id: Id<K>) -> u32 {
        self.find(id).ok().and_then(|at| self.list.get(at)).map_or(0, |held| held.count)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// 昇順で、数が 0 の項目が無いか。
    pub(crate) fn well_formed(&self) -> bool {
        self.list.iter().all(|held| held.count > 0)
            && self.list.as_slice().windows(2).all(|pair| match pair {
                [a, b] => a.id < b.id,
                [] | [_] | [_, _, _, ..] => false,
            })
    }
}

impl TryClone for Process {
    fn try_clone(&self) -> Result<Process, Error> {
        let Process { life, quota, exit_signal, threads, objects, backings, senders, receivers, space, ios, stale_runners } = self;
        Ok(Process {
            life: *life,
            quota: *quota,
            exit_signal: *exit_signal,
            threads: threads.try_clone()?,
            objects: objects.try_clone()?,
            backings: backings.try_clone()?,
            senders: senders.try_clone()?,
            receivers: receivers.try_clone()?,
            space: space.try_clone()?,
            ios: *ios,
            stale_runners: *stale_runners,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Process {
    pub(crate) life: Life,
    pub(crate) quota: Quota,
    pub(crate) exit_signal: ObjectId,
    pub(crate) threads: List<ThreadId>,
    pub(crate) objects: Holdings<kind::Object>,
    pub(crate) backings: Holdings<kind::Backing>,
    /// 通信路の送る端の保持。
    pub(crate) senders: Holdings<kind::Channel>,
    /// 通信路の受ける端の保持。
    pub(crate) receivers: Holdings<kind::Channel>,
    pub(crate) space: Space,
    /// このプロセスが持つ I/O 要求の数。
    pub(crate) ios: u32,
    /// 権限を減らす変更より前の世代で実行中のスレッドの数。
    pub(crate) stale_runners: u32,
}

macro_rules! quota {
    ($($(#[$doc:meta])* $name:ident / $with:ident : $ty:ty;)*) => {
        /// 一つのプロセスへの割り当て。[`crate::Limits`] が状態全体の上限を決め、これは
        /// その中で一つのプロセスが使える量を決める。効くのは、二つのうち小さい方である。
        ///
        /// 割り当てを今の使用量より小さくしても、既にあるものは取り上げない。それ以上
        /// 増やす操作を、状態を変えずに `Exhausted` で拒否する。
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct Quota {
            $($name: $ty,)*
        }

        impl Quota {
            /// 割り当てなし (状態全体の上限だけが効く)。作った直後のプロセスの値。
            pub const UNLIMITED: Quota = Quota { $($name: <$ty>::MAX,)* };

            $(
                $(#[$doc])*
                #[must_use]
                pub const fn $name(&self) -> $ty {
                    self.$name
                }

                $(#[$doc])*
                #[must_use]
                pub const fn $with(mut self, value: $ty) -> Quota {
                    self.$name = value;
                    self
                }
            )*
        }
    };
}

quota! {
    /// スレッドの数。超えると `Exhausted(ProcessThreads)`。
    threads / with_threads: u32;
    /// I/O 要求の数 (回収されるまで数える)。超えると `Exhausted(ProcessIos)`。
    ios / with_ios: u32;
    /// 参照できる資源の種類数 (待機可能な資源、実体、通信路の端のそれぞれ)。超えると `Exhausted(Holdings)`。
    holdings / with_holdings: u32;
    /// アドレス空間のマッピング数。超えると `Exhausted(Mappings)`。
    mappings / with_mappings: u32;
    /// 実体つきで対応づけているバイト数。超えると `Exhausted(MappedBytes)`。
    mapped_bytes / with_mapped_bytes: u64;
}

impl Kernel {
    /// プロセスへの割り当てを変える。変える前の値を返す。
    pub fn set_quota(&mut self, process: ProcessId, quota: Quota) -> Result<Quota, Error> {
        let entry = self.processes.find_mut(process, &self.lineage)?;
        Ok(core::mem::replace(&mut entry.quota, quota))
    }

    /// プロセスへの割り当て。
    pub fn quota(&self, process: ProcessId) -> Result<Quota, Error> {
        Ok(self.processes.find(process, &self.lineage)?.quota)
    }

    /// `process` が参照できる資源の種類数: 状態全体の上限と、プロセスへの割り当ての小さい方。
    pub(crate) fn holdings_limit(&self, process: ProcessId) -> u32 {
        let quota = self.processes.get(process).map_or(u32::MAX, |entry| entry.quota.holdings);
        self.limits.holdings().min(quota)
    }

    /// `process` のアドレス空間のマッピング数: 状態全体の上限と、プロセスへの割り当ての小さい方。
    pub(crate) fn mappings_limit(&self, process: ProcessId) -> u32 {
        let quota = self.processes.get(process).map_or(u32::MAX, |entry| entry.quota.mappings);
        self.limits.mappings().min(quota)
    }

    /// プロセスと、その終了時に数が 1 になる資源を作る。
    ///
    /// 終了通知の資源は誰にも保持されていない。待つ側のプロセスが
    /// [`Kernel::retain_object`] で保持する。
    pub fn create_process(&mut self, width: AddrWidth, page: PageSize) -> Result<ProcessCreated, Error> {
        self.processes.reserve()?;
        self.objects.reserve()?;
        // 終了通知の資源の破棄、プロセスの終了、プロセスの回収。
        self.effects.promise(3)?;
        let exit_signal = self.objects.insert(Object::new(SignalSpec::new(SignalMode::Keep, 0, 1), 0, 1))?;
        let process = self.processes.insert(Process {
            life: Life::Alive,
            quota: Quota::UNLIMITED,
            exit_signal,
            threads: List::new(),
            objects: Holdings::new(),
            backings: Holdings::new(),
            senders: Holdings::new(),
            receivers: Holdings::new(),
            space: Space::new(width, page),
            ios: 0,
            stale_runners: 0,
        })?;
        Ok((process, exit_signal))
    }

    /// プロセスを終了させる。実行中でないスレッドはここで終了し、実行中のスレッドは
    /// 実行権を返した時点で終了する。
    pub fn exit_process(&mut self, process: ProcessId, code: ExitCode) -> Result<(), Error> {
        let entry = self.processes.find_mut(process, &self.lineage)?;
        if entry.life != Life::Alive {
            return Err(Error::Refused(Refusal::ProcessExiting));
        }
        entry.life = Life::Exiting(code);
        let mut at = 0usize;
        loop {
            let Some(entry) = self.processes.get(process) else { return Ok(()) };
            let Some(thread) = entry.threads.get(at).copied() else { break };
            let running = {
                let thread = self.threads.get_mut(thread).ok_or(Error::Broken(Invariant::ProcessTree))?;
                match thread.run {
                    Run::Running { .. } => {
                        thread.kill = thread.kill.or(Some(code));
                        true
                    }
                    Run::Ready | Run::Servicing | Run::Blocked(_) => false,
                }
            };
            if running {
                self.interrupt(thread)?;
                at = at.saturating_add(1);
            } else {
                self.exit_thread_now(thread, code)?;
                if self.threads.contains(thread) {
                    return Err(Error::Broken(Invariant::ProcessTree));
                }
            }
        }
        let idle = self
            .processes
            .get(process)
            .is_some_and(|entry| entry.threads.is_empty() && matches!(entry.life, Life::Exiting(_)));
        if idle { self.finish_process(process) } else { Ok(()) }
    }

    /// プロセスの寿命の段階。
    pub fn process_phase(&self, process: ProcessId) -> Result<ProcessPhase, Error> {
        let entry = self.processes.find(process, &self.lineage)?;
        Ok(match entry.life {
            Life::Alive => ProcessPhase::Alive,
            Life::Exiting(_) => ProcessPhase::Exiting,
            Life::Draining(_) => ProcessPhase::Draining,
        })
    }

    /// プロセスに所属するスレッドの数。
    pub fn thread_count(&self, process: ProcessId) -> Result<usize, Error> {
        Ok(self.processes.find(process, &self.lineage)?.threads.len())
    }

    /// スレッドがすべて止まったプロセスの資源を手放し、終了を通知する。
    pub(crate) fn finish_process(&mut self, process: ProcessId) -> Result<(), Error> {
        let entry = self.processes.get(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
        let Life::Exiting(code) = entry.life else { return Err(Error::Broken(Invariant::ProcessTree)) };
        if !entry.threads.is_empty() || entry.stale_runners != 0 {
            return Err(Error::Broken(Invariant::ProcessTree));
        }
        let exit_signal = entry.exit_signal;
        self.drop_timers_of(process)?;
        self.abandon_ios_of(process)?;
        loop {
            let entry = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
            let Some(object) = entry.objects.take_one() else { break };
            self.drop_handle(object)?;
        }
        loop {
            let entry = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
            let Some(backing) = entry.backings.take_one() else { break };
            self.drop_backing_hold(backing)?;
        }
        loop {
            let entry = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
            let Some(channel) = entry.senders.take_one() else { break };
            self.drop_end(channel, End::Send)?;
        }
        loop {
            let entry = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
            let Some(channel) = entry.receivers.take_one() else { break };
            self.drop_end(channel, End::Receive)?;
        }
        self.clear_space(process)?;
        self.raise(exit_signal)?;
        self.unpin(exit_signal)?;
        self.effects.emit(Effect::ProcessExited { process, code })?;
        let entry = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
        entry.life = Life::Draining(code);
        self.reclaim_process_if_drained(process)
    }

    /// 未完了 I/O が無くなった `Draining` のプロセスを回収する。
    pub(crate) fn reclaim_process_if_drained(&mut self, process: ProcessId) -> Result<(), Error> {
        let entry = self.processes.get(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
        if !matches!(entry.life, Life::Draining(_)) || entry.ios != 0 {
            return Ok(());
        }
        self.processes.remove(process).ok_or(Error::Broken(Invariant::ProcessTree))?;
        self.effects.emit(Effect::ProcessReclaimed { process })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::Arena;

    #[test]
    fn holdings_notice_a_zero_count_and_a_wrong_order() {
        let mut arena = Arena::<kind::Object, ()>::new(crate::InstanceId::new(1), 8);
        let a = arena.insert(()).unwrap();
        let b = arena.insert(()).unwrap();
        let mut held = Holdings::new();
        assert!(held.is_empty() && held.well_formed());
        held.add(a, 8).unwrap();
        held.add(b, 8).unwrap();
        assert!(!held.is_empty() && held.well_formed());
        let mut zero = held.try_clone().unwrap();
        zero.corrupt_first_count(0);
        assert!(!zero.well_formed());
        // 同じ ID が二度並ぶ (昇順は「より小さい」でなければならない)。
        let mut twice = Holdings::new();
        twice.list.push(Held { id: a, count: 1 }).unwrap();
        twice.list.push(Held { id: a, count: 1 }).unwrap();
        assert!(!twice.well_formed());
        let mut reversed = Holdings::new();
        reversed.list.push(Held { id: b, count: 1 }).unwrap();
        reversed.list.push(Held { id: a, count: 1 }).unwrap();
        assert!(!reversed.well_formed());
    }

    #[test]
    fn holdings_count_and_stay_sorted() {
        let mut arena = Arena::<kind::Object, ()>::new(crate::InstanceId::new(1), 8);
        let a = arena.insert(()).unwrap();
        let b = arena.insert(()).unwrap();
        let mut held = Holdings::new();
        held.add(b, 8).unwrap();
        held.add(a, 8).unwrap();
        held.add(b, 8).unwrap();
        assert!(held.well_formed());
        assert_eq!((held.count_of(a), held.count_of(b)), (1, 2));
        held.sub(b).unwrap();
        held.sub(b).unwrap();
        assert_eq!(held.sub(b), Err(Error::Refused(Refusal::NotHeld)));
        assert_eq!(held.take_one(), Some(a));
        assert!(held.is_empty());
    }

    #[test]
    fn holdings_limit_counts_kinds_not_references() {
        let mut arena = Arena::<kind::Object, ()>::new(crate::InstanceId::new(1), 8);
        let a = arena.insert(()).unwrap();
        let b = arena.insert(()).unwrap();
        let mut held = Holdings::new();
        held.add(a, 1).unwrap();
        held.add(a, 1).unwrap();
        assert_eq!(held.add(b, 1), Err(Error::Exhausted(Resource::Holdings)));
        assert_eq!(held.count_of(a), 2);
    }
}
