//! 同期の部品。並行の検査 (`--cfg loom`) のときは Loom のものに差し替わる。

#[cfg(loom)]
pub(crate) use loom::sync::{Arc, Condvar, Mutex, MutexGuard, atomic::AtomicU64, atomic::Ordering};
#[cfg(not(loom))]
pub(crate) use std::sync::{Arc, Condvar, Mutex, MutexGuard, atomic::AtomicU64, atomic::Ordering};
