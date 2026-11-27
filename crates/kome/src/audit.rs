//! 不変条件の検査。状態から数え直して、記録と一致するかを確かめる。
//!
//! 台帳 (`docs/invariants.toml`) の各条件のうち、一つの状態を見て判定できるものを
//! ここで検査する。試験と決定的シミュレーションは、すべての操作の後にこれを呼ぶ。
//! 履歴にかかわる条件 (終端は一度だけ、など) は、遷移規則の網羅試験・Kani・TLA+ と、
//! testkit の履歴の監視が受け持つ。

use crate::error::Invariant;
use crate::id::{BackingId, ObjectId, ProcessId};
use crate::io::IoState;
use crate::kernel::{Due, Kernel};
use crate::mem::MapSource;
use crate::addr::Prot;
use crate::process::Life;
use crate::thread::Run;

/// 破れていた不変条件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Violation {
    invariant: Invariant,
    detail: &'static str,
}

impl Violation {
    /// 破れていた条件。
    #[must_use]
    pub const fn invariant(&self) -> Invariant {
        self.invariant
    }

    /// 何が一致しなかったか。
    #[must_use]
    pub const fn detail(&self) -> &'static str {
        self.detail
    }
}

const fn ensure(holds: bool, invariant: Invariant, detail: &'static str) -> Result<(), Violation> {
    if holds { Ok(()) } else { Err(Violation { invariant, detail }) }
}

fn count(items: impl Iterator) -> u64 {
    u64::try_from(items.count()).unwrap_or(u64::MAX)
}

impl Kernel {
    /// すべての不変条件を検査する。最初に見つかった違反を返す。
    pub fn audit(&self) -> Result<(), Violation> {
        self.audit_ids()?;
        self.audit_capacity()?;
        self.audit_runs()?;
        self.audit_processes()?;
        self.audit_ios()?;
        self.audit_objects()?;
        self.audit_waits()?;
        self.audit_time()?;
        self.audit_memory()?;
        self.audit_effects()
    }

    fn audit_ids(&self) -> Result<(), Violation> {
        let consistent = self.processes.consistent()
            && self.threads.consistent()
            && self.objects.consistent()
            && self.backings.consistent()
            && self.timers.consistent()
            && self.ios.consistent();
        ensure(consistent, Invariant::IdFresh, "表の空きスロットと生存数が一致しない")
    }

    /// 確保しない操作 (解放、終了、起床、実行権の貸し出し、`Effect` の発行) が使う容量が、
    /// 予約した数だけ残っているか。どの操作が予約し、どの操作が使うかは `docs/capacity.tsv` にある。
    fn audit_capacity(&self) -> Result<(), Violation> {
        let arenas = self.processes.reserved()
            && self.threads.reserved()
            && self.objects.reserved()
            && self.backings.reserved()
            && self.timers.reserved()
            && self.ios.reserved();
        ensure(arenas, Invariant::IdFresh, "表が、取り除いた番号を戻すための容量を持っていない")?;
        let workers = usize::try_from(self.limits.workers()).unwrap_or(usize::MAX);
        ensure(
            self.running.capacity() >= workers,
            Invariant::RunOnePermit,
            "実行権の表が、worker の上限ぶんの容量を持っていない",
        )?;
        let threads = usize::try_from(self.threads.len()).unwrap_or(usize::MAX);
        ensure(
            self.ready.capacity() >= threads,
            Invariant::RunQueue,
            "実行待ちの列が、スレッドの数ぶんの容量を持っていない",
        )?;
        ensure(self.effects.reserved(), Invariant::EffectsKept, "Effect のキューが、上限ぶんの容量を持っていない")
    }

    fn audit_runs(&self) -> Result<(), Violation> {
        let lent = self.running.as_slice();
        ensure(
            count(lent.iter()) <= u64::from(self.limits.workers()),
            Invariant::RunOnePermit,
            "貸し出しが worker の上限を超えている",
        )?;
        for (at, entry) in lent.iter().enumerate() {
            let earlier = lent.iter().take(at);
            ensure(
                earlier.clone().all(|other| other.worker != entry.worker),
                Invariant::RunOnePermit,
                "一つの worker が二つの実行権を持っている",
            )?;
            ensure(
                earlier.clone().all(|other| other.thread != entry.thread),
                Invariant::RunOnePermit,
                "一つのスレッドに二つの実行権がある",
            )?;
            let matches = self.threads.get(entry.thread).is_some_and(
                |thread| matches!(thread.run, Run::Running { worker, .. } if worker == entry.worker),
            );
            ensure(matches, Invariant::RunOnePermit, "貸し出しの記録とスレッドの状態が一致しない")?;
        }
        for (id, thread) in self.threads.iter() {
            let queued = count(self.ready.iter().filter(|queued| **queued == id));
            match thread.run {
                Run::Running { .. } => {
                    ensure(
                        lent.iter().any(|entry| entry.thread == id),
                        Invariant::RunOnePermit,
                        "実行中のスレッドに貸し出しの記録が無い",
                    )?;
                    ensure(queued == 0, Invariant::RunQueue, "実行中のスレッドが実行待ちの列にある")?;
                }
                Run::Ready => {
                    let expected = u64::from(thread.suspend == 0);
                    ensure(queued == expected, Invariant::RunQueue, "実行可能なスレッドと実行待ちの列が一致しない")?;
                    ensure(thread.kill.is_none(), Invariant::RunQueue, "終了要求が実行待ちのスレッドに残っている")?;
                }
                Run::Servicing | Run::Blocked(_) => {
                    ensure(queued == 0, Invariant::RunQueue, "実行できないスレッドが実行待ちの列にある")?;
                    ensure(thread.kill.is_none(), Invariant::RunQueue, "終了要求が止まっているスレッドに残っている")?;
                }
            }
        }
        ensure(
            self.ready.iter().all(|queued| self.threads.contains(*queued)),
            Invariant::RunQueue,
            "実行待ちの列に、終了したスレッドがある",
        )
    }

    fn audit_processes(&self) -> Result<(), Violation> {
        for (id, thread) in self.threads.iter() {
            let listed = self
                .processes
                .get(thread.process)
                .map_or(0, |process| count(process.threads.iter().filter(|member| **member == id)));
            ensure(listed == 1, Invariant::ProcessTree, "スレッドが所属プロセスの一覧にちょうど一度現れない")?;
        }
        for (id, process) in self.processes.iter() {
            ensure(
                process.threads.iter().all(|member| self.threads.get(*member).is_some_and(|t| t.process == id)),
                Invariant::ProcessTree,
                "プロセスの一覧に、所属していないスレッドがある",
            )?;
            ensure(
                process.objects.well_formed() && process.backings.well_formed(),
                Invariant::ObjectRefs,
                "プロセスの参照の記録が整列していない",
            )?;
            let ios = count(self.ios.iter().filter(|(_, io)| io.process == id));
            ensure(u64::from(process.ios) == ios, Invariant::ProcessTree, "プロセスの I/O 数が一致しない")?;
            let stale = count(self.threads.iter().filter(|(_, thread)| {
                thread.process == id && matches!(thread.run, Run::Running { stale: true, .. })
            }));
            ensure(u64::from(process.stale_runners) == stale, Invariant::MemBarrier, "古い世代の実行の数が一致しない")?;
            ensure(
                process.space.limbo.is_empty() || stale > 0,
                Invariant::MemBarrier,
                "古い世代の実行が無いのに保留が残っている",
            )?;
            match process.life {
                Life::Alive => {}
                Life::Exiting(_) => {
                    let all_dying = process.threads.iter().all(|member| {
                        self.threads
                            .get(*member)
                            .is_some_and(|t| matches!(t.run, Run::Running { .. }) && t.kill.is_some())
                    });
                    ensure(
                        !process.threads.is_empty() && all_dying,
                        Invariant::ProcessTree,
                        "終了中のプロセスに、終了要求の無いスレッドがある",
                    )?;
                }
                Life::Draining(_) => {
                    let empty = process.threads.is_empty()
                        && process.objects.is_empty()
                        && process.backings.is_empty()
                        && process.space.maps.is_empty();
                    ensure(empty, Invariant::ProcessTree, "終了済みのプロセスが資源を持っている")?;
                    let in_flight = self
                        .ios
                        .iter()
                        .filter(|(_, io)| io.process == id)
                        .all(|(_, io)| io.state == IoState::CancelRequested);
                    ensure(
                        in_flight && ios > 0,
                        Invariant::ProcessTree,
                        "終了済みのプロセスに、取消を要求していない I/O がある",
                    )?;
                    ensure(
                        !self.timers.iter().any(|(_, timer)| timer.process == id),
                        Invariant::ProcessTree,
                        "終了済みのプロセスがタイマを持っている",
                    )?;
                }
            }
        }
        Ok(())
    }

    fn pins_of(&self, object: ObjectId) -> u64 {
        let waits = count(self.threads.iter().filter(
            |(_, thread)| matches!(&thread.run, Run::Blocked(wait) if wait.targets.iter().any(|t| *t == object)),
        ));
        let exits = count(self.threads.iter().filter(|(_, thread)| thread.exit_signal == object));
        let process_exits = count(self.processes.iter().filter(|(_, process)| {
            process.exit_signal == object && !matches!(process.life, Life::Draining(_))
        }));
        let timers = count(self.timers.iter().filter(|(_, timer)| timer.object == object));
        let ios: u64 = self
            .ios
            .iter()
            .map(|(_, io)| u64::from(io.target == Some(object)).saturating_add(u64::from(io.notify == Some(object))))
            .fold(0u64, u64::saturating_add);
        waits.saturating_add(exits).saturating_add(process_exits).saturating_add(timers).saturating_add(ios)
    }

    fn audit_objects(&self) -> Result<(), Violation> {
        for (id, object) in self.objects.iter() {
            let handles: u64 = self
                .processes
                .iter()
                .map(|(_, process)| u64::from(process.objects.count_of(id)))
                .fold(0u64, u64::saturating_add);
            ensure(u64::from(object.handles) == handles, Invariant::ObjectRefs, "資源の保持数が一致しない")?;
            ensure(u64::from(object.pins) == self.pins_of(id), Invariant::ObjectRefs, "資源の固定数が一致しない")?;
            ensure(handles > 0 || object.pins > 0, Invariant::ObjectRefs, "参照の無い資源が残っている")?;
            ensure(object.count <= object.max, Invariant::ObjectRefs, "資源の数が上限を超えている")?;
        }
        Ok(())
    }

    fn audit_waits(&self) -> Result<(), Violation> {
        for (id, thread) in self.threads.iter() {
            let Run::Blocked(wait) = &thread.run else { continue };
            for (at, target) in wait.targets.iter().enumerate() {
                let listed =
                    self.objects.get(*target).map_or(0, |o| count(o.waiters.iter().filter(|w| **w == id)));
                ensure(listed == 1, Invariant::WaitOnce, "待機が対象の待機列にちょうど一度現れない")?;
                ensure(
                    wait.targets.iter().take(at).all(|earlier| earlier != target),
                    Invariant::WaitOnce,
                    "待機の対象が重複している",
                )?;
            }
            let satisfiable = self.evaluate(wait.targets.as_slice(), wait.mode).ok().flatten().is_some();
            ensure(!satisfiable, Invariant::WaitNoLostWake, "成立している待機が登録されたまま残っている")?;
            if let Some((at, seq)) = wait.due {
                let listed = count(
                    self.due.iter().filter(|e| e.at == at && e.seq == seq && e.what == Due::Wait(id)),
                );
                ensure(listed == 1, Invariant::WaitOnce, "待機の期限が期限の列にちょうど一度現れない")?;
                ensure(at > self.now, Invariant::WaitOnce, "期限を過ぎた待機が登録されたまま残っている")?;
            }
        }
        for (id, object) in self.objects.iter() {
            let real = object.waiters.iter().all(|waiter| {
                self.threads.get(*waiter).is_some_and(
                    |thread| matches!(&thread.run, Run::Blocked(wait) if wait.targets.iter().any(|t| *t == id)),
                )
            });
            ensure(real, Invariant::WaitOnce, "待機列に、この資源を待っていないスレッドがある")?;
        }
        Ok(())
    }

    fn audit_time(&self) -> Result<(), Violation> {
        let sorted = self.due.as_slice().windows(2).all(|pair| match pair {
            [a, b] => (a.at, a.seq) <= (b.at, b.seq),
            [] | [_] | [_, _, _, ..] => false,
        });
        ensure(sorted, Invariant::TimeOrder, "期限の列が (時刻, 登録順) の順でない")?;
        for entry in self.due.iter() {
            let real = match entry.what {
                Due::Wait(thread) => self.threads.get(thread).is_some_and(
                    |t| matches!(&t.run, Run::Blocked(wait) if wait.due == Some((entry.at, entry.seq))),
                ),
                Due::Timer(timer) => self
                    .timers
                    .get(timer)
                    .and_then(|t| t.armed)
                    .is_some_and(|armed| armed.at == entry.at && armed.seq == entry.seq),
            };
            ensure(real, Invariant::TimeOrder, "期限の列に、対応する待機もタイマも無い項目がある")?;
        }
        let armed = count(self.timers.iter().filter(|(_, timer)| timer.armed.is_some()));
        let waits = count(
            self.threads.iter().filter(|(_, t)| matches!(&t.run, Run::Blocked(wait) if wait.due.is_some())),
        );
        ensure(
            count(self.due.iter()) == armed.saturating_add(waits),
            Invariant::TimeOrder,
            "期限の列の長さが、設定済みのタイマと期限付きの待機の数に一致しない",
        )?;
        for (_, timer) in self.timers.iter() {
            ensure(
                self.processes.contains(timer.process) && self.objects.contains(timer.object),
                Invariant::TimeOrder,
                "タイマが、存在しないプロセスまたは資源を指している",
            )?;
        }
        Ok(())
    }

    fn backing_refs(&self, backing: BackingId) -> (u64, u64, u64) {
        let mut holds = 0u64;
        let mut maps = 0u64;
        let mut limbo = 0u64;
        for (_, process) in self.processes.iter() {
            holds = holds.saturating_add(u64::from(process.backings.count_of(backing)));
            maps = maps.saturating_add(count(process.space.maps.iter().filter(
                |mapping| matches!(mapping.source, MapSource::Backing { backing: mapped, .. } if mapped == backing),
            )));
            limbo = limbo.saturating_add(count(process.space.limbo.iter().filter(|held| **held == backing)));
        }
        (holds, maps, limbo)
    }

    fn audit_layout(&self, process: ProcessId) -> Result<(), Violation> {
        let Some(process) = self.processes.get(process) else { return Ok(()) };
        let space = &process.space;
        let mut previous: Option<u64> = None;
        for mapping in space.maps.iter() {
            let ordered = mapping.start <= mapping.last && previous.is_none_or(|last| last < mapping.start);
            ensure(ordered, Invariant::MemLayout, "マッピングが昇順でない、または重なっている")?;
            previous = Some(mapping.last);
            let aligned = space.page.divides(mapping.start) && space.page.divides(mapping.last.wrapping_add(1));
            ensure(aligned, Invariant::MemLayout, "マッピングが粒度に揃っていない")?;
            ensure(mapping.last <= space.width.last().get(), Invariant::MemLayout, "マッピングが幅を超えている")?;
            match mapping.source {
                MapSource::Reserved => {
                    ensure(mapping.prot == Prot::NONE, Invariant::MemLayout, "予約だけのマッピングが権限を持っている")?;
                }
                MapSource::Backing { backing, offset } => {
                    let inside = self.backings.get(backing).is_some_and(|entry| {
                        mapping
                            .last
                            .checked_sub(mapping.start)
                            .and_then(|span| offset.get().checked_add(span))
                            .is_some_and(|end| end < entry.len)
                    });
                    ensure(inside, Invariant::MemBackingAlive, "マッピングが、解放済みの実体または実体の外を指している")?;
                    ensure(space.page.divides(offset.get()), Invariant::MemLayout, "実体の中の位置が揃っていない")?;
                }
            }
        }
        ensure(
            count(space.maps.iter()) <= u64::from(self.limits.mappings()),
            Invariant::MemLayout,
            "マッピングの数が上限を超えている",
        )?;
        ensure(
            space.limbo.iter().all(|backing| self.backings.contains(*backing)),
            Invariant::MemBackingAlive,
            "保留が、解放済みの実体を指している",
        )
    }

    fn audit_memory(&self) -> Result<(), Violation> {
        for (id, _) in self.processes.iter() {
            self.audit_layout(id)?;
        }
        let mut bytes = 0u64;
        for (id, backing) in self.backings.iter() {
            let (holds, maps, limbo) = self.backing_refs(id);
            let matches =
                u64::from(backing.holds) == holds && u64::from(backing.maps) == maps && u64::from(backing.limbo) == limbo;
            ensure(matches, Invariant::MemBackingAlive, "実体の参照の数が一致しない")?;
            ensure(
                holds.saturating_add(maps).saturating_add(limbo) > 0,
                Invariant::MemBackingAlive,
                "参照の無い実体が残っている",
            )?;
            bytes = bytes.saturating_add(backing.len);
        }
        ensure(
            bytes == self.backing_bytes && bytes <= self.limits.backing_bytes(),
            Invariant::MemBackingAlive,
            "実体の合計バイト数が一致しない",
        )
    }

    fn audit_ios(&self) -> Result<(), Violation> {
        for (_, io) in self.ios.iter() {
            let alive = self.processes.contains(io.process)
                && [io.target, io.notify].into_iter().flatten().all(|object| self.objects.contains(object));
            ensure(alive, Invariant::IoOnce, "I/O が、存在しないプロセスまたは資源を指している")?;
        }
        Ok(())
    }

    /// 予約しているはずの `Effect` の席を状態から数え直す。
    fn expected_promises(&self) -> u64 {
        let processes: u64 = self
            .processes
            .iter()
            .map(|(_, process)| match process.life {
                Life::Alive | Life::Exiting(_) => 2u64,
                Life::Draining(_) => 1,
            })
            .fold(0u64, u64::saturating_add);
        let threads: u64 = self
            .threads
            .iter()
            .map(|(_, thread)| match &thread.run {
                Run::Running { interrupted: false, .. } | Run::Blocked(_) => 2u64,
                Run::Running { interrupted: true, .. } | Run::Ready | Run::Servicing => 1,
            })
            .fold(0u64, u64::saturating_add);
        let ios: u64 =
            self.ios.iter().map(|(_, io)| u64::from(io.state.promises())).fold(0u64, u64::saturating_add);
        processes
            .saturating_add(threads)
            .saturating_add(ios)
            .saturating_add(u64::from(self.objects.len()))
            .saturating_add(u64::from(self.backings.len()))
    }

    fn audit_effects(&self) -> Result<(), Violation> {
        ensure(
            u64::from(self.effects.promised()) == self.expected_promises(),
            Invariant::EffectsKept,
            "Effect の予約数が、状態から数えた数と一致しない",
        )?;
        ensure(self.effects.within_limit(), Invariant::EffectsKept, "Effect のキューと予約が上限を超えている")
    }
}

