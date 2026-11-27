//! 仮想単調時刻。kome は実時計を読まない。時刻は [`crate::Kernel::advance`] で与えられる。
//!
//! 周期タイマの周期は [`Period`] で、0 を表せない。0 の周期は同じ時刻に通知を繰り返し、
//! `advance` が終わらなくなる。下の二つは、周期を `Period` で渡すか `Duration` で渡すかだけが違う。
//!
//! ```
//! fn arm(kernel: &mut kome::Kernel, timer: kome::TimerId, period: kome::Period) {
//!     let _ = kernel.arm_timer(timer, kome::Instant::ZERO, Some(period));
//! }
//! ```
//!
//! ```compile_fail
//! fn arm(kernel: &mut kome::Kernel, timer: kome::TimerId, period: kome::Duration) {
//!     let _ = kernel.arm_timer(timer, kome::Instant::ZERO, Some(period));
//! }
//! ```
//!
//! 周期の下限も `Period` で、0 にできない。
//!
//! ```
//! let _ = kome::Limits::DEFAULT.with_min_period(kome::Period::MIN);
//! ```
//!
//! ```compile_fail
//! let _ = kome::Limits::DEFAULT.with_min_period(kome::Duration::from_nanos(0));
//! ```

use core::num::NonZeroU64;

/// 仮想単調時刻 (起点からのナノ秒)。日時や時差は Personality が持つ。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant(u64);

impl Instant {
    /// 起点。
    pub const ZERO: Instant = Instant(0);

    /// 起点からのナノ秒で作る。
    #[must_use]
    pub const fn from_nanos(nanos: u64) -> Instant {
        Instant(nanos)
    }

    /// 起点からのナノ秒。
    #[must_use]
    pub const fn nanos(self) -> u64 {
        self.0
    }

    /// `span` だけ後の時刻。表現できなければ `None`。
    #[must_use]
    pub const fn checked_add(self, span: Duration) -> Option<Instant> {
        match self.0.checked_add(span.0) {
            Some(nanos) => Some(Instant(nanos)),
            None => None,
        }
    }
}

/// 時間の長さ (ナノ秒)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Duration(u64);

impl Duration {
    /// ナノ秒で作る。
    #[must_use]
    pub const fn from_nanos(nanos: u64) -> Duration {
        Duration(nanos)
    }

    /// ナノ秒。
    #[must_use]
    pub const fn nanos(self) -> u64 {
        self.0
    }
}

/// 周期タイマの周期。0 より長い。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Period(NonZeroU64);

impl Period {
    /// 表せる最も短い周期 (1 ナノ秒)。
    pub const MIN: Period = Period(NonZeroU64::MIN);

    /// 長さから作る。長さが 0 なら `None`。
    #[must_use]
    pub const fn new(span: Duration) -> Option<Period> {
        match NonZeroU64::new(span.0) {
            Some(nanos) => Some(Period(nanos)),
            None => None,
        }
    }

    /// 周期の長さ。
    #[must_use]
    pub const fn get(self) -> Duration {
        Duration(self.0.get())
    }
}

/// 待機の期限。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Deadline {
    /// この時刻になったら時間切れにする。
    At(Instant),
    /// 期限なし。
    Never,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_refuses_overflow() {
        let late = Instant::from_nanos(u64::MAX - 1);
        assert_eq!(late.checked_add(Duration::from_nanos(1)), Some(Instant::from_nanos(u64::MAX)));
        assert_eq!(late.checked_add(Duration::from_nanos(2)), None);
    }

    #[test]
    fn a_period_is_never_zero() {
        assert_eq!(Period::new(Duration::from_nanos(0)), None);
        assert_eq!(Period::new(Duration::from_nanos(1)), Some(Period::MIN));
        assert_eq!(Period::new(Duration::from_nanos(7)).map(Period::get), Some(Duration::from_nanos(7)));
        // 周期を足した時刻は、表現できる限り必ず後になる。
        for start in [0, 1, u64::MAX - 1] {
            let next = Instant::from_nanos(start).checked_add(Period::MIN.get());
            assert_eq!(next, Some(Instant::from_nanos(start + 1)));
        }
        assert_eq!(Instant::from_nanos(u64::MAX).checked_add(Period::MIN.get()), None);
    }
}
