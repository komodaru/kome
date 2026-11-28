//! 資源の上限。kome の表はすべてここで上限を持つ。

use core::num::NonZeroU32;

use crate::sched::Budget;
use crate::time::{Duration, Period};

macro_rules! limits {
    ($($(#[$doc:meta])* $name:ident / $with:ident : $ty:ty = $default:expr;)*) => {
        /// 一つの [`crate::Kernel`] が持てる資源の上限。
        ///
        /// 製品の品質設定や利用方針から値を決めるのは kome の外の仕事である。
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct Limits {
            $($name: $ty,)*
        }

        impl Limits {
            /// 試験と小さな構成のための既定値。
            pub const DEFAULT: Limits = Limits { $($name: $default,)* };

            $(
                $(#[$doc])*
                #[must_use]
                pub const fn $name(&self) -> $ty {
                    self.$name
                }

                $(#[$doc])*
                #[must_use]
                pub const fn $with(mut self, value: $ty) -> Limits {
                    self.$name = value;
                    self
                }
            )*

            /// 全ての上限の名前と値。記録が、どの上限で動かしたかを残すのに使う。
            pub fn each(&self, mut visit: impl FnMut(&'static str, u64)) {
                $(visit(stringify!($name), Word::word(self.$name));)*
            }

            /// 名前で一つの上限を置き換える。その名前の上限が無いか、値が型に収まらなければ `None`。
            #[must_use]
            pub fn with_named(mut self, name: &str, value: u64) -> Option<Limits> {
                $(if name == stringify!($name) {
                    self.$name = Word::from_word(value)?;
                    return Some(self);
                })*
                None
            }
        }
    };
}

/// 一つの整数として書き出せる値。上限と割り当てを、名前と数の組として記録に残すのに使う。
pub(crate) trait Word: Sized {
    fn word(self) -> u64;
    fn from_word(word: u64) -> Option<Self>;
}

impl Word for u64 {
    fn word(self) -> u64 {
        self
    }

    fn from_word(word: u64) -> Option<u64> {
        Some(word)
    }
}

impl Word for u32 {
    fn word(self) -> u64 {
        u64::from(self)
    }

    fn from_word(word: u64) -> Option<u32> {
        u32::try_from(word).ok()
    }
}

impl Word for NonZeroU32 {
    fn word(self) -> u64 {
        u64::from(self.get())
    }

    fn from_word(word: u64) -> Option<NonZeroU32> {
        NonZeroU32::new(u32::try_from(word).ok()?)
    }
}

impl Word for Budget {
    fn word(self) -> u64 {
        self.get()
    }

    fn from_word(word: u64) -> Option<Budget> {
        Some(Budget::new(word))
    }
}

impl Word for Period {
    fn word(self) -> u64 {
        self.get().nanos()
    }

    fn from_word(word: u64) -> Option<Period> {
        Period::new(Duration::from_nanos(word))
    }
}

limits! {
    /// プロセスの数。
    processes / with_processes: u32 = 16;
    /// スレッドの数。
    threads / with_threads: u32 = 256;
    /// 待機可能な資源の数。
    objects / with_objects: u32 = 4096;
    /// メモリの実体の数。
    backings / with_backings: u32 = 1024;
    /// 実体の合計バイト数。
    backing_bytes / with_backing_bytes: u64 = 1 << 32;
    /// タイマの数。
    timers / with_timers: u32 = 256;
    /// I/O 要求の数。
    ios / with_ios: u32 = 1024;
    /// 通信路の数。
    channels / with_channels: u32 = 1024;
    /// 通信路が持てるバイト数の合計。
    channel_bytes / with_channel_bytes: u64 = 1 << 26;
    /// 同時に実行権を持てる worker の数。
    workers / with_workers: u32 = 1;
    /// ホストが取り出すまで保持できる `Effect` の数 (予約を含む)。
    effects / with_effects: u32 = 16384;
    /// 一つのアドレス空間のマッピング数。
    mappings / with_mappings: u32 = 4096;
    /// 一つの待機の対象数。
    wait_targets / with_wait_targets: u32 = 64;
    /// 一つのプロセスが参照できる資源の種類数 (待機可能な資源と実体のそれぞれ)。
    holdings / with_holdings: u32 = 4096;
    /// 一度の実行権で実行してよい量。0 の実行権は貸さない。
    quantum / with_quantum: Budget = Budget::new(100_000);
    /// 周期タイマの周期の下限。これより短い周期は拒否する。0 にはできない。
    min_period / with_min_period: Period = DEFAULT_MIN_PERIOD;
    /// 一度の [`crate::Kernel::advance`] が処理する期限の数の目安。これだけ処理したら、同じ時刻の
    /// 期限を処理し終えたところで返る。0 にはできない。
    advance_batch / with_advance_batch: NonZeroU32 = DEFAULT_ADVANCE_BATCH;
}

const DEFAULT_MIN_PERIOD: Period = match Period::new(Duration::from_nanos(100_000)) {
    Some(period) => period,
    None => Period::MIN,
};

const DEFAULT_ADVANCE_BATCH: NonZeroU32 = match NonZeroU32::new(4096) {
    Some(batch) => batch,
    None => NonZeroU32::MIN,
};

impl Default for Limits {
    fn default() -> Limits {
        Limits::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_limit_is_written_and_read_back_by_name() {
        // どの上限も既定値と違う値にして、名前と数の組から同じものを作り直す。
        let mut changed = Limits::DEFAULT;
        let mut names = Vec::new();
        Limits::DEFAULT.each(|name, value| {
            changed = changed.with_named(name, value / 2 + 3).unwrap();
            names.push(name);
        });
        let mut rebuilt = Limits::DEFAULT;
        changed.each(|name, value| {
            assert_ne!(Limits::DEFAULT.with_named(name, value), Some(Limits::DEFAULT), "{name}");
            rebuilt = rebuilt.with_named(name, value).unwrap();
        });
        assert_eq!(rebuilt, changed);
        assert_eq!(names.len(), 17);
        // 無い名前と、型に収まらない値は断る。
        assert_eq!(Limits::DEFAULT.with_named("nothing", 1), None);
        assert_eq!(Limits::DEFAULT.with_named("threads", u64::MAX), None);
        assert_eq!(Limits::DEFAULT.with_named("min_period", 0), None);
        assert_eq!(Limits::DEFAULT.with_named("advance_batch", 0), None);
    }

    #[test]
    fn with_changes_only_that_limit() {
        let limits = Limits::DEFAULT.with_threads(3);
        assert_eq!(limits.threads(), 3);
        assert_eq!(limits.with_threads(Limits::DEFAULT.threads()), Limits::DEFAULT);
    }
}
