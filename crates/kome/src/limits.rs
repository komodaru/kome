//! 資源の上限。kome の表はすべてここで上限を持つ。

use crate::sched::Budget;
use crate::time::Duration;

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
        }
    };
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
    /// 周期タイマの周期の下限。これより短い周期は拒否する。
    min_period / with_min_period: Duration = Duration::from_nanos(100_000);
}

impl Default for Limits {
    fn default() -> Limits {
        Limits::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_changes_only_that_limit() {
        let limits = Limits::DEFAULT.with_threads(3);
        assert_eq!(limits.threads(), 3);
        assert_eq!(limits.with_threads(Limits::DEFAULT.threads()), Limits::DEFAULT);
    }
}
