//! kome: 複数の OS Personality に共通するライブラリカーネル。
//!
//! プロセス、スレッド、アドレス空間、資源の寿命、待機、タイマ、非同期 I/O の
//! 状態と状態遷移を持つ。個々の OS の関数名・構造体・ホストの機能は持たない。
//!
//! # 契約
//!
//! - **単一 writer**: 一つの [`Kernel`] は一つの実行主体が `&mut` で更新する。
//!   更新に入る情報は、メソッドの引数 ([`Input`])、ホストからの [`Completion`]、
//!   [`Kernel::advance`] に渡す仮想時刻だけである。時計・乱数・ファイルは読まない。
//! - **外部処理は [`Effect`]**: 受理した操作が必要とする `Effect` の席は受理前に
//!   予約する。キューが満杯なら受理前に [`Error::Exhausted`] を返し、受理済みの
//!   操作の `Effect` は失わない。
//! - **失敗は状態を変えない**: [`Error::Stale`]・[`Error::Refused`]・
//!   [`Error::Exhausted`]・[`Error::Foreign`] を返した呼び出しは `Kernel` を変更しない。
//!   [`Error::Broken`] だけは内部の不変条件違反 (kome の誤り) を表す。
//! - **確保の失敗も返す**: 表はすべて [`Limits`] で上限を持ち、確保は失敗を返せる
//!   経路だけを使う。ホストのメモリ不足は `Exhausted(Resource::HostMemory)` になる。
//!   解放・終了・起床の経路は確保しない。
//! - **状態は一つ**: [`Kernel`] は複製できない。[`Kernel::snapshot`] の写しは、実行権を
//!   貸しておらず、応答待ちの I/O も無いときに限り、別の実行インスタンスとして復元できる
//!   ([`Snapshot::restore`])。復元前の実行権と完了通知は [`Error::Foreign`] になる。
//! - **決定的**: 同じ初期状態と同じ入力列から同じ状態と同じ `Effect` 列を得る。
//!   走査順は ID のスロット順か登録順で、ハッシュ表は使わない。
//!
//! 不変条件の台帳は `docs/invariants.toml`、検査は [`Kernel::audit`]。
#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod addr;
mod audit;
mod effect;
mod error;
mod id;
mod input;
mod io;
mod ipc;
mod kernel;
mod limits;
mod mem;
mod object;
mod process;
#[cfg(kani)]
mod proofs;
mod sched;
mod store;
#[cfg(test)]
mod tests;
mod thread;
mod time;
mod timer;
mod wait;

pub use addr::{Access, AddrWidth, BackingOffset, GuestAddr, GuestRange, PageSize, Prot};
pub use audit::Violation;
pub use effect::{Completion, CompletionKind, Effect};
pub use error::{Error, Invariant, Refusal, Resource};
pub use id::{BackingId, ChannelId, Id, IoId, Kind, ObjectId, ProcessId, Tag, ThreadId, TimerId, kind};
pub use input::{Input, Reply};
pub use ipc::{ChannelCreated, ChannelSpec, End, Parcel, Received, Sent};
pub use io::{CancelReply, HostError, IoLinks, IoOutcome, IoPhase, IoSettled};
pub use kernel::{Census, InstanceId, Kernel, Snapshot};
pub use limits::Limits;
pub use mem::{MapGen, MapSource, MappingInfo, MemFault, MemFaultKind, Span};
pub use object::{SignalMode, SignalOp, SignalSpec};
pub use process::{ExitCode, ProcessCreated, ProcessPhase};
pub use sched::{NotRelinquished, Budget, RunGen, RunPermit, Stop, ThreadFate, WorkerId};
pub use thread::{Start, ThreadCreated, ThreadPhase};
pub use time::{Deadline, Duration, Instant, Period};
pub use timer::Advanced;
pub use wait::{Interruptible, WaitMode, WaitOutcome, WaitStart};
