//! 同期の部品。並行の検査のときは、検査の道具のものに差し替わる: `--cfg loom` なら Loom (進み方を
//! 全て試す)、`--cfg shuttle` なら Shuttle (種から決まる進み方を数多く試し、同じ種で再現する)。

#[cfg(loom)]
pub(crate) use loom::sync::{Arc, Condvar, Mutex, MutexGuard, atomic::AtomicU64, atomic::Ordering};
#[cfg(shuttle)]
pub(crate) use shuttle::sync::{Arc, Condvar, Mutex, MutexGuard, atomic::AtomicU64, atomic::Ordering};
#[cfg(not(any(loom, shuttle)))]
pub(crate) use std::sync::{Arc, Condvar, Mutex, MutexGuard, atomic::AtomicU64, atomic::Ordering};
