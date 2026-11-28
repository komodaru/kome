//! メモリ backend との境界: 内容の読み書き、アドレス空間の変更の通知、JIT のコードの無効化。
//!
//! kome が持つのは、範囲・対応・権限・世代である。内容のバイト列は backend が持つ。backend の
//! 作り方 (ゲストのアドレスをそのままホストの連続領域に置く、アクセスごとに引き直す、書かれた
//! ページだけを持つ) は kome の外で決める。ここで決めるのは、どの作り方でも守る約束である。
//!
//! - **内容** ([`Contents`]): 実体の ID と、実体の中の位置で読み書きする。ゲストのアドレスは渡さない。
//! - **基準の経路** ([`guest_read`]、[`guest_write`]): ゲストのアドレスでのアクセスを
//!   [`Kernel::translate`] で引き、マッピングの境界をまたぐ範囲を区間ごとに読み書きする。速い経路
//!   (アドレスを直接使う、引いた結果を覚えておく) は、これと同じ結果にならなければならない。
//! - **変更の通知** ([`SpaceObserver`]): アドレス空間を変える操作は [`map_observed`]、
//!   [`unmap_observed`]、[`protect_observed`] を通して呼ぶ。backend は、通知だけを見ていれば
//!   kome と同じアドレス空間を保てる (ホスト側のページ表、引いた結果の覚え書きを更新できる)。
//! - **コードの無効化** ([`CodeCache`]、[`CodeWatch`]): 変換済みのコードを持つ実行器は、マッピングが
//!   外れた範囲と権限が変わった範囲のコードを捨てる。実行できる権限で見せている実体への書き込みは、
//!   どの範囲から書いても [`guest_write_watched`] が知らせる。実行器が自分の速い経路で書く場合は、
//!   実行器が同じことをする。
//!
//! 通知は、変更を受理した直後に届く。古い世代で実行中の worker がいても、実体の内容は
//! [`crate::Effect::BackingReclaimed`] が出るまで手放してはならない (台帳 MEM-1)。

use crate::addr::{Access, BackingOffset, GuestAddr, GuestRange, Prot};
use crate::error::Error;
use crate::id::{BackingId, ProcessId};
use crate::io::HostError;
use crate::kernel::Kernel;
use crate::mem::{MapGen, MapSource, MemFault, MemFaultKind, Span};

/// 実体の内容を持つもの。
pub trait Contents {
    /// 実体 `backing` の `offset` から、`out` の長さぶんを読む。
    fn read(&self, backing: BackingId, offset: BackingOffset, out: &mut [u8]) -> Result<(), HostError>;

    /// 実体 `backing` の `offset` へ `data` を書く。
    fn write(&mut self, backing: BackingId, offset: BackingOffset, data: &[u8]) -> Result<(), HostError>;
}

/// ゲストのアドレスでのアクセスが成功しなかった理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessError {
    /// kome の呼び出しの失敗 (古い ID など)。
    Kernel(Error),
    /// ゲストのアクセスの失敗。範囲の中で最初に失敗したアドレスを持つ。何も読み書きしていない。
    Fault(MemFault),
    /// backend が内容を読み書きできなかった。書き込みは途中まで済んでいることがある。
    Host(HostError),
}

/// 範囲を、連続してアクセスできる区間に分けて順に渡す。`visit` は (区間, 済んだバイト数, この区間のバイト数) を受け取る。
fn walk(
    kernel: &Kernel,
    process: ProcessId,
    addr: GuestAddr,
    len: usize,
    access: Access,
    mut visit: impl FnMut(Span, usize, usize) -> Result<(), HostError>,
) -> Result<(), AccessError> {
    if len == 0 {
        // 長さ 0 のアクセスも、プロセスの ID は確かめる (古い ID での呼び出しを成功にしない)。
        kernel.map_generation(process).map_err(AccessError::Kernel)?;
    }
    let mut done = 0usize;
    while done < len {
        let rest = len.saturating_sub(done);
        let start = u64::try_from(done).ok().and_then(|done| addr.get().checked_add(done));
        let range = start.and_then(|start| GuestRange::new(GuestAddr::new(start), u64::try_from(rest).ok()?));
        // アドレスの端を越える範囲は、越える手前の位置で失敗する。
        let Some(range) = range else {
            let at = GuestAddr::new(start.unwrap_or(u64::MAX));
            return Err(AccessError::Fault(MemFault::new(at, MemFaultKind::Unmapped)));
        };
        let span = kernel.translate(process, range, access).map_err(AccessError::Kernel)?.map_err(AccessError::Fault)?;
        let take = usize::try_from(span.bytes()).unwrap_or(usize::MAX).min(rest);
        visit(span, done, take).map_err(AccessError::Host)?;
        done = done.saturating_add(take);
    }
    Ok(())
}

/// `process` のアドレス `addr` から、`out` の長さぶんを読む。基準の経路。
///
/// 範囲のどこかが読めなければ、何も読まずに、最初に失敗したアドレスを返す。
pub fn guest_read<C: Contents + ?Sized>(
    kernel: &Kernel,
    contents: &C,
    process: ProcessId,
    addr: GuestAddr,
    out: &mut [u8],
) -> Result<(), AccessError> {
    walk(kernel, process, addr, out.len(), Access::Read, |_, _, _| Ok(()))?;
    walk(kernel, process, addr, out.len(), Access::Read, |span, done, take| {
        match out.get_mut(done..done.saturating_add(take)) {
            Some(part) => contents.read(span.backing(), span.offset(), part),
            None => Ok(()),
        }
    })
}

/// `process` のアドレス `addr` へ `data` を書く。基準の経路。
///
/// 範囲のどこかが書けなければ、何も書かずに、最初に失敗したアドレスを返す。
pub fn guest_write<C: Contents + ?Sized>(
    kernel: &Kernel,
    contents: &mut C,
    process: ProcessId,
    addr: GuestAddr,
    data: &[u8],
) -> Result<(), AccessError> {
    walk(kernel, process, addr, data.len(), Access::Write, |_, _, _| Ok(()))?;
    walk(kernel, process, addr, data.len(), Access::Write, |span, done, take| {
        match data.get(done..done.saturating_add(take)) {
            Some(part) => contents.write(span.backing(), span.offset(), part),
            None => Ok(()),
        }
    })
}

/// [`guest_write`] と同じ。書いた内容を実行できる権限で見せているマッピングがあれば、そのコードを捨てさせる。
///
/// Personality や Host がゲストのメモリへ書く経路 (読み込んだファイルの内容、再配置) に使う。
///
/// - 見るのは書き込み先の範囲の権限ではなく、書いた実体である。同じ実体を別の範囲や別のプロセスへ
///   実行できる権限で見せていれば (別名)、そちらのコードも捨てさせる ([`Kernel::executable_views`])。
/// - backend が途中で失敗しても ([`AccessError::Host`])、内容は一部変わっているかもしれないので、
///   範囲の全体について捨てさせてから失敗を返す。何も書いていない失敗 ([`AccessError::Fault`]) では捨てない。
pub fn guest_write_watched<C: Contents + ?Sized, K: CodeCache + ?Sized>(
    kernel: &Kernel,
    contents: &mut C,
    code: &mut K,
    process: ProcessId,
    addr: GuestAddr,
    data: &[u8],
) -> Result<(), AccessError> {
    walk(kernel, process, addr, data.len(), Access::Write, |_, _, _| Ok(()))?;
    let written = walk(kernel, process, addr, data.len(), Access::Write, |span, done, take| {
        match data.get(done..done.saturating_add(take)) {
            Some(part) => contents.write(span.backing(), span.offset(), part),
            None => Ok(()),
        }
    });
    let mut failed = None;
    walk(kernel, process, addr, data.len(), Access::Write, |span, _, take| {
        let len = u64::try_from(take).unwrap_or(u64::MAX);
        let seen = kernel.executable_views(span.backing(), span.offset(), len, |shown_in, range| code.invalidate(shown_in, range));
        if let Err(error) = seen {
            failed.get_or_insert(error);
        }
        Ok(())
    })?;
    written?;
    failed.map_or(Ok(()), |error| Err(AccessError::Kernel(error)))
}

/// アドレス空間の一つの変更。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapChange {
    /// 空いていた範囲へマッピングを置いた。予約だけのマッピングの権限は [`Prot::NONE`]。
    Mapped {
        /// 置いた範囲。
        range: GuestRange,
        /// 中身。
        source: MapSource,
        /// 権限。
        prot: Prot,
    },
    /// 範囲の中のマッピングを全て外した (範囲の中に、元から何も無い部分があってもよい)。
    Unmapped {
        /// 外した範囲。
        range: GuestRange,
    },
    /// 範囲の権限を変えた。
    Protected {
        /// 変えた範囲。
        range: GuestRange,
        /// 新しい権限。
        prot: Prot,
    },
}

impl MapChange {
    /// 変わった範囲。
    #[must_use]
    pub const fn range(&self) -> GuestRange {
        match self {
            MapChange::Mapped { range, .. } | MapChange::Unmapped { range } | MapChange::Protected { range, .. } => *range,
        }
    }

    /// この変更の前に変換したコードを、捨てなければならないか。
    ///
    /// 空いていた範囲へ置く変更では、その範囲に変換済みのコードは無い。外す変更と権限の変更では、
    /// 範囲の中のコードは実行できなくなったか、これから書き換えられるかもしれない。
    #[must_use]
    pub const fn invalidates_code(&self) -> bool {
        match self {
            MapChange::Mapped { .. } => false,
            MapChange::Unmapped { .. } | MapChange::Protected { .. } => true,
        }
    }
}

/// アドレス空間の変更を受け取るもの (メモリ backend、引いた結果の覚え書き、JIT のコード)。
pub trait SpaceObserver {
    /// `process` のアドレス空間が `change` のとおりに変わり、世代が `generation` になった。
    fn changed(&mut self, process: ProcessId, generation: MapGen, change: MapChange);

    /// `process` のアドレス空間が無くなった。[`crate::Effect::ProcessExited`] を受けたホストが呼ぶ。
    fn cleared(&mut self, process: ProcessId);
}

/// 借りている受け取り手も、受け取り手である。
impl<O: SpaceObserver + ?Sized> SpaceObserver for &mut O {
    fn changed(&mut self, process: ProcessId, generation: MapGen, change: MapChange) {
        (**self).changed(process, generation, change);
    }

    fn cleared(&mut self, process: ProcessId) {
        (**self).cleared(process);
    }
}

/// 二つの受け取り手へ、同じ順に知らせる。
impl<A: SpaceObserver, B: SpaceObserver> SpaceObserver for (A, B) {
    fn changed(&mut self, process: ProcessId, generation: MapGen, change: MapChange) {
        self.0.changed(process, generation, change);
        self.1.changed(process, generation, change);
    }

    fn cleared(&mut self, process: ProcessId) {
        self.0.cleared(process);
        self.1.cleared(process);
    }
}

/// [`Kernel::map`] を呼び、受理されたら `observer` へ知らせる。
pub fn map_observed<O: SpaceObserver + ?Sized>(
    kernel: &mut Kernel,
    observer: &mut O,
    process: ProcessId,
    range: GuestRange,
    source: MapSource,
    prot: Prot,
) -> Result<MapGen, Error> {
    let generation = kernel.map(process, range, source, prot)?;
    let prot = match source {
        MapSource::Reserved => Prot::NONE,
        MapSource::Backing { .. } => prot,
    };
    observer.changed(process, generation, MapChange::Mapped { range, source, prot });
    Ok(generation)
}

/// [`Kernel::unmap`] を呼び、何かを外したら `observer` へ知らせる。
pub fn unmap_observed<O: SpaceObserver + ?Sized>(
    kernel: &mut Kernel,
    observer: &mut O,
    process: ProcessId,
    range: GuestRange,
) -> Result<u64, Error> {
    let bytes = kernel.unmap(process, range)?;
    if bytes != 0 {
        observer.changed(process, kernel.map_generation(process)?, MapChange::Unmapped { range });
    }
    Ok(bytes)
}

/// [`Kernel::protect`] を呼び、何かが変わったら `observer` へ知らせる。
pub fn protect_observed<O: SpaceObserver + ?Sized>(
    kernel: &mut Kernel,
    observer: &mut O,
    process: ProcessId,
    range: GuestRange,
    prot: Prot,
) -> Result<MapGen, Error> {
    let before = kernel.map_generation(process)?;
    let generation = kernel.protect(process, range, prot)?;
    if generation != before {
        observer.changed(process, generation, MapChange::Protected { range, prot });
    }
    Ok(generation)
}

/// 変換済みのコードを持つもの (JIT のコードキャッシュ、解釈の覚え書き)。
pub trait CodeCache {
    /// `process` の `range` に重なるコードを捨てる。
    fn invalidate(&mut self, process: ProcessId, range: GuestRange);

    /// `process` のコードを全て捨てる。
    fn forget(&mut self, process: ProcessId);
}

/// [`CodeCache`] を、アドレス空間の変更の受け取り手にする。
#[derive(Debug)]
pub struct CodeWatch<K> {
    cache: K,
}

impl<K: CodeCache> CodeWatch<K> {
    /// `cache` を包む。
    #[must_use]
    pub const fn new(cache: K) -> CodeWatch<K> {
        CodeWatch { cache }
    }

    /// 包んでいるもの。
    pub const fn cache(&mut self) -> &mut K {
        &mut self.cache
    }
}

impl<K: CodeCache> SpaceObserver for CodeWatch<K> {
    fn changed(&mut self, process: ProcessId, _generation: MapGen, change: MapChange) {
        if change.invalidates_code() {
            self.cache.invalidate(process, change.range());
        }
    }

    fn cleared(&mut self, process: ProcessId) {
        self.cache.forget(process);
    }
}
