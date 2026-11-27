//! 失敗の分類。ゲストへ返せる拒否、資源不足、別インスタンス、内部の不変条件違反を
//! 一つの値へ潰さない (台帳 ERR-1)。

use crate::id::Tag;

/// [`crate::Kernel`] の操作が返す失敗。
///
/// `Broken` 以外は、呼び出しが状態を変更しなかったことを意味する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// ID が生きている資源を指していない (解放済み、または世代が違う)。
    Stale(Tag),
    /// 要求は現在の状態では受理できない。Personality がゲストのエラーへ対応づける。
    Refused(Refusal),
    /// 上限またはホストのメモリが足りない。受理前の失敗であり、後で再試行できる。
    Exhausted(Resource),
    /// 別の実行インスタンスの実行権または完了通知。
    Foreign,
    /// 内部の不変条件が破れている。kome の誤りであり、ゲストのエラーにしてはならない。
    Broken(Invariant),
}

/// 状態が要求を許さない理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// 上限の組み合わせが成立しない (実行量が 0)。
    BadLimits,
    /// 復元先の実行インスタンスが、写しを取った実行インスタンスと同じである。
    SameInstance,
    /// 実行権を貸したままの写しは復元できない (貸した実行権は復元先では返せない)。
    PermitsLent,
    /// プロセスが終了処理に入っている。
    ProcessExiting,
    /// スレッドが、この操作に必要な「サービス中」の状態にない。
    NotServicing,
    /// 停止要求が無いのに停止解除を求めた。
    NotSuspended,
    /// 停止要求の数が表現の上限に達している。
    SuspendOverflow,
    /// スレッドは待機していない。
    NotWaiting,
    /// 待機は割り込みを受け付けない条件で登録されている。
    NotInterruptible,
    /// 待機対象に同じ資源が二度ある。
    DuplicateTarget,
    /// 実行権は既に返されたか、取り消された実行のものである。
    StalePermit,
    /// worker は既に実行権を持っている。
    WorkerBusy,
    /// プロセスはこの資源への参照を持っていない。
    NotHeld,
    /// 参照の数が表現の上限に達している。
    HoldOverflow,
    /// 信号の数が資源の上限を超える。
    SignalOverflow,
    /// 信号の定義が矛盾している (上限が 0、初期値が上限より大きい)。
    BadSignalSpec,
    /// アドレスまたは長さが粒度に揃っていない。
    Misaligned,
    /// 範囲がアドレス空間の幅に収まらない。
    OutOfSpace,
    /// 範囲が既存のマッピングと重なる。
    Overlap,
    /// 範囲に、マッピングの無い部分がある。
    NotMapped,
    /// 範囲に、実体の無い予約だけの部分がある。
    NotBacked,
    /// 実体の範囲を超える。
    BeyondBacking,
    /// 長さ 0 の実体は作れない。
    EmptyBacking,
    /// 仮想時刻を過去へ戻そうとした。
    TimeReversed,
    /// 周期が下限より短い。
    PeriodTooShort,
    /// I/O は発行前であり、完了通知を受け取れない。
    IoNotIssued,
    /// I/O は既に発行されている。
    IoAlreadyIssued,
    /// I/O の終端結果は既に確定している。
    IoSettled,
    /// I/O の終端結果がまだ確定していない (発行済みで、ホストの応答を待っている)。
    IoInFlight,
    /// I/O の終端結果は確定していない、または既に公開済みである。
    IoNotPublishable,
    /// 通信路の定義が成立しない (持てる数が 0、合計のバイト数が表現できない)。
    BadChannelSpec,
    /// 通信路のその端は、保持が無くなって閉じている。
    EndClosed,
    /// メッセージが、通信路の一つのメッセージの上限より長い。
    MessageTooLong,
    /// 受け取るバッファが、先頭のメッセージより短い。
    BufferTooSmall,
    /// バイトの流れは資源を運べない (境界が無いので、どのバイトに付くかが決まらない)。
    ParcelOnStream,
    /// この資源の数は kome が決める (通信路の通知)。外からは変えられない。
    DrivenObject,
    /// 実行量を使い切ったとして返したが、実行した量が貸した量に届いていない。
    BudgetRemains,
    /// 中断の要求に応じたとして返したが、その実行に中断を要求していない。
    NotInterrupted,
    /// 実行権を貸せる状態 (実行待ちで、停止要求が無い) ではない。
    NotReady,
    /// 権限を減らす変更の後で、古い世代の実行が残っているプロセスには貸せない。
    Barrier,
    /// 実行中ではない。
    NotRunning,
    /// その実行の文脈は、既に載っているか、既にある。
    ContextBusy,
    /// そのスレッドの文脈が置かれていない。
    NoContext,
}

/// 足りなくなった資源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    /// プロセスの数。
    Processes,
    /// スレッドの数。
    Threads,
    /// 待機可能な資源の数。
    Objects,
    /// メモリの実体の数。
    Backings,
    /// 実体の合計バイト数。
    BackingBytes,
    /// タイマの数。
    Timers,
    /// I/O 要求の数。
    Ios,
    /// 通信路の数。
    Channels,
    /// 通信路が持てるバイト数の合計。
    ChannelBytes,
    /// 同時に実行権を持てる worker の数。
    Workers,
    /// `Effect` のキュー。ホストが取り出せば空く。
    Effects,
    /// 一つのアドレス空間のマッピング数。
    Mappings,
    /// 一つの待機の対象数。
    WaitTargets,
    /// 一つのプロセスが参照できる資源の種類数。
    Holdings,
    /// 資源を使用中として固定する参照の数。
    Pins,
    /// スロット番号、または順序番号の空間。
    Identifiers,
    /// ホストのメモリ (確保が失敗した)。
    HostMemory,
}

/// 台帳 (`docs/invariants.toml`) に載る不変条件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invariant {
    /// RUN-1: 一つのスレッドに有効な実行権は一つ。一つの worker が持つ実行権も一つ。
    RunOnePermit,
    /// RUN-2: 実行待ち行列は、実行可能なスレッドをちょうど一度ずつ含む。
    RunQueue,
    /// ID-1: 解放済みの内部 ID は、新しい資源を指さない。
    IdFresh,
    /// IO-1: 終端済みの I/O は再度終端しない。
    IoOnce,
    /// WAIT-1: 待機結果は一つだけ確定する (待機の登録は対象・期限と一対一に対応する)。
    WaitOnce,
    /// WAIT-2: 成立している待機が登録されたまま残らない (起床を失わない)。
    WaitNoLostWake,
    /// MEM-1: 使用中の実体を解放しない (参照の数が実際の参照と一致する)。
    MemBackingAlive,
    /// MEM-2: マッピングは整列し、重ならず、アドレス空間の幅に収まる。
    MemLayout,
    /// MEM-3: 権限を減らす変更の後、古い世代の実行が残る間はそのプロセスへ実行権を貸さない。
    MemBarrier,
    /// FX-1: キューが満杯でも、受理済み操作の `Effect` を失わない (予約数が一致する)。
    EffectsKept,
    /// OBJ-1: 資源の参照数と固定数は、実際の保持者の数と一致する。
    ObjectRefs,
    /// TIME-1: 期限の列は (時刻, 登録順) の順で、待機・タイマと一対一に対応する。
    TimeOrder,
    /// PROC-1: プロセスとスレッドの所属関係は相互に一致し、終了済みのプロセスは資源を持たない。
    ProcessTree,
    /// ERR-1: ゲストへ返す拒否と、ランタイム内部の障害を混同しない。
    ErrorClasses,
    /// IPC-1: 通信路の中身は容量を超えず、端の保持と通知は実際の状態と一致する。
    ChannelKept,
}

impl Invariant {
    /// すべての不変条件。台帳との照合に使う。
    pub const ALL: [Invariant; 15] = [
        Invariant::RunOnePermit,
        Invariant::RunQueue,
        Invariant::IdFresh,
        Invariant::IoOnce,
        Invariant::WaitOnce,
        Invariant::WaitNoLostWake,
        Invariant::MemBackingAlive,
        Invariant::MemLayout,
        Invariant::MemBarrier,
        Invariant::EffectsKept,
        Invariant::ObjectRefs,
        Invariant::TimeOrder,
        Invariant::ProcessTree,
        Invariant::ErrorClasses,
        Invariant::ChannelKept,
    ];

    /// 台帳での識別子。
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Invariant::RunOnePermit => "RUN-1",
            Invariant::RunQueue => "RUN-2",
            Invariant::IdFresh => "ID-1",
            Invariant::IoOnce => "IO-1",
            Invariant::WaitOnce => "WAIT-1",
            Invariant::WaitNoLostWake => "WAIT-2",
            Invariant::MemBackingAlive => "MEM-1",
            Invariant::MemLayout => "MEM-2",
            Invariant::MemBarrier => "MEM-3",
            Invariant::EffectsKept => "FX-1",
            Invariant::ObjectRefs => "OBJ-1",
            Invariant::TimeOrder => "TIME-1",
            Invariant::ProcessTree => "PROC-1",
            Invariant::ErrorClasses => "ERR-1",
            Invariant::ChannelKept => "IPC-1",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique() {
        for (i, a) in Invariant::ALL.iter().enumerate() {
            for b in &Invariant::ALL[i + 1..] {
                assert_ne!(a.code(), b.code());
            }
        }
    }
}
