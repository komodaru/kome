//! 仮想単調時刻。kome は実時計を読まない。時刻は [`crate::Kernel::advance`] で与えられる。

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
}
