//! 実体の内容の置き場。

use std::collections::BTreeMap;

use kome::{BackingId, BackingOffset, Contents, HostError};

/// 置き場が返す失敗の番号。
pub mod fault {
    use kome::HostError;

    /// その実体は置き場に無い。
    pub const NO_BACKING: HostError = HostError::new(1);
    /// 実体の長さを超えている。
    pub const OUT_OF_BOUNDS: HostError = HostError::new(2);
    /// 同じ実体が既にある。
    pub const DUPLICATE: HostError = HostError::new(3);
    /// 確保できなかった。
    pub const NO_MEMORY: HostError = HostError::new(4);
}

/// 実体ごとに連続したバイト列を持つ置き場。
///
/// kome が `Effect` で実体の回収を知らせたら [`FlatStore::retire`] で捨てる。
#[derive(Debug, Default)]
pub struct FlatStore {
    backings: BTreeMap<BackingId, Vec<u8>>,
}

impl FlatStore {
    /// 空の置き場。
    #[must_use]
    pub const fn new() -> FlatStore {
        FlatStore { backings: BTreeMap::new() }
    }

    /// 0 で埋めた `len` バイトの実体を置く。
    pub fn create(&mut self, backing: BackingId, len: u64) -> Result<(), HostError> {
        if self.backings.contains_key(&backing) {
            return Err(fault::DUPLICATE);
        }
        let len = usize::try_from(len).map_err(|_| fault::NO_MEMORY)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(len).map_err(|_| fault::NO_MEMORY)?;
        bytes.resize(len, 0);
        self.backings.insert(backing, bytes);
        Ok(())
    }

    /// 実体を捨てる。あったかを返す。
    pub fn retire(&mut self, backing: BackingId) -> bool {
        self.backings.remove(&backing).is_some()
    }

    /// 置いてある実体の数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.backings.len()
    }

    /// 実体が一つも無いか。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.backings.is_empty()
    }
}

/// `offset` から `len` バイトの、バイト列の中での範囲。
fn span(offset: BackingOffset, len: usize) -> Option<core::ops::Range<usize>> {
    let start = usize::try_from(offset.get()).ok()?;
    Some(start..start.checked_add(len)?)
}

impl Contents for FlatStore {
    fn read(&self, backing: BackingId, offset: BackingOffset, out: &mut [u8]) -> Result<(), HostError> {
        let bytes = self.backings.get(&backing).ok_or(fault::NO_BACKING)?;
        let from = span(offset, out.len()).and_then(|span| bytes.get(span)).ok_or(fault::OUT_OF_BOUNDS)?;
        out.copy_from_slice(from);
        Ok(())
    }

    fn write(&mut self, backing: BackingId, offset: BackingOffset, data: &[u8]) -> Result<(), HostError> {
        let bytes = self.backings.get_mut(&backing).ok_or(fault::NO_BACKING)?;
        let to = span(offset, data.len()).and_then(|span| bytes.get_mut(span)).ok_or(fault::OUT_OF_BOUNDS)?;
        to.copy_from_slice(data);
        Ok(())
    }
}
