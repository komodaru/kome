//! Kani による証明。`cargo kani -p kome` (scripts/ci/kani.sh) で動かす。
//!
//! 証明には二種類ある。どちらであるかと前提は、証明ごとに `docs/proofs.tsv` に書く
//! (`kome-lint` の ledger が、この表と照合する)。
//!
//! - 整数だけの関数 (範囲、時刻、周期、I/O の遷移、席の勘定) は、引数の全ての値について示す。
//! - 表やキューを使う証明 (ID、`Effect` の発行) は、小さな上限と、決まった長さの操作の列に
//!   限って示す。記憶域の確保が成功した場合だけを対象にする。上限や列の長さを変えた場合は
//!   示していない。その部分は試験と決定的シミュレーションが見る。

use core::num::NonZeroU32;

use crate::addr::{GuestAddr, GuestRange, PageSize};
use crate::effect::{Effect, Effects, Seats};
use crate::error::Error;
use crate::id::{Arena, IoId, kind};
use crate::io::{HostError, IoEmit, IoEvent, IoOutcome, IoState, io_step};
use crate::time::{Duration, Instant, Period};

fn any_range() -> GuestRange {
    let range = GuestRange::spanning(GuestAddr::new(kani::any()), GuestAddr::new(kani::any()));
    kani::assume(range.is_some());
    range.unwrap()
}

/// 範囲は回り込まない。作れるのは、長さが 1 以上で末尾が `u64` に収まるときだけである。
#[kani::proof]
fn range_new_never_wraps() {
    let start: u64 = kani::any();
    let len: u64 = kani::any();
    let fits = len != 0 && u128::from(start) + u128::from(len) - 1 <= u128::from(u64::MAX);
    match GuestRange::new(GuestAddr::new(start), len) {
        Some(range) => {
            assert!(fits);
            assert!(range.start().get() == start);
            assert!(range.last().get() >= start);
            assert!(u128::from(range.last().get()) == u128::from(start) + u128::from(len) - 1);
            assert!(range.byte_len() == Some(len));
        }
        None => assert!(!fits),
    }
}

/// バイト数が表現できないのは、範囲が `u64` 全体のときだけである。
#[kani::proof]
fn range_len_is_none_only_for_everything() {
    let range = any_range();
    let whole = range.start().get() == 0 && range.last().get() == u64::MAX;
    assert!(range.byte_len().is_none() == whole);
    if let Some(len) = range.byte_len() {
        assert!(GuestRange::new(range.start(), len) == Some(range));
    }
}

/// 「重なる」は「共有するバイトがある」と同じである。
#[kani::proof]
fn range_overlap_is_a_shared_byte() {
    let a = any_range();
    let b = any_range();
    let addr = GuestAddr::new(kani::any());
    if a.contains(addr) && b.contains(addr) {
        assert!(a.overlaps(b));
    }
    assert!(a.overlaps(b) == b.overlaps(a));
    if a.overlaps(b) {
        let shared = GuestAddr::new(a.start().get().max(b.start().get()));
        assert!(a.contains(shared) && b.contains(shared));
    }
}

/// 粒度の倍数の判定は、剰余が 0 であることと同じである。
#[kani::proof]
fn page_size_divides_is_remainder_zero() {
    let bytes: u64 = kani::any();
    let value: u64 = kani::any();
    match PageSize::new(bytes) {
        Some(page) => {
            assert!(bytes != 0 && bytes & (bytes - 1) == 0);
            assert!(page.divides(value) == (value % bytes == 0));
        }
        None => assert!(bytes == 0 || bytes & (bytes - 1) != 0),
    }
}

/// 時刻の加算は回り込まない。
#[kani::proof]
fn instant_add_never_wraps() {
    let at: u64 = kani::any();
    let span: u64 = kani::any();
    match Instant::from_nanos(at).checked_add(Duration::from_nanos(span)) {
        Some(later) => assert!(later.nanos() >= at && later.nanos() - at == span),
        None => assert!(u128::from(at) + u128::from(span) > u128::from(u64::MAX)),
    }
}

/// 周期を足した次の期限は、表現できる限り必ず今の期限より後になる (周期タイマは同じ時刻に留まらない)。
#[kani::proof]
fn a_period_moves_the_deadline_forward() {
    let at: u64 = kani::any();
    let span: u64 = kani::any();
    match Period::new(Duration::from_nanos(span)) {
        Some(period) => match Instant::from_nanos(at).checked_add(period.get()) {
            Some(next) => assert!(next > Instant::from_nanos(at)),
            None => assert!(u128::from(at) + u128::from(span) > u128::from(u64::MAX)),
        },
        None => assert!(span == 0),
    }
}

fn any_outcome() -> IoOutcome {
    let transferred: u64 = kani::any();
    match kani::any::<u8>() % 3 {
        0 => IoOutcome::Done { transferred },
        1 => IoOutcome::Failed { error: HostError::new(kani::any()), transferred },
        _ => IoOutcome::Cancelled { transferred },
    }
}

fn any_state() -> IoState {
    match kani::any::<u8>() % 5 {
        0 => IoState::Prepared,
        1 => IoState::Issued,
        2 => IoState::CancelRequested,
        3 => IoState::Settled(any_outcome()),
        _ => IoState::Published(any_outcome()),
    }
}

fn any_event() -> IoEvent {
    match kani::any::<u8>() % 4 {
        0 => IoEvent::Issue,
        1 => IoEvent::Cancel,
        2 => IoEvent::Complete(any_outcome()),
        _ => IoEvent::Publish,
    }
}

const fn outcome_of(state: IoState) -> Option<IoOutcome> {
    match state {
        IoState::Prepared | IoState::Issued | IoState::CancelRequested => None,
        IoState::Settled(outcome) | IoState::Published(outcome) => Some(outcome),
    }
}

/// 終端結果は一度確定したら変わらない (台帳 IO-1)。どの出来事の列でも成り立つよう、
/// 一歩について全ての状態と出来事を調べる。
#[kani::proof]
fn io_outcome_is_settled_once() {
    let before = any_state();
    let event = any_event();
    if let Ok((after, _)) = io_step(before, event) {
        if let Some(outcome) = outcome_of(before) {
            assert!(outcome_of(after) == Some(outcome));
        }
        // 完了通知を受理するのは、発行済みで終端していない要求だけである。
        if matches!(event, IoEvent::Complete(_)) {
            assert!(matches!(before, IoState::Issued | IoState::CancelRequested));
        }
    }
}

/// 遷移が出す `Effect` は、遷移前の状態が予約していた席に収まる (台帳 FX-1)。
#[kani::proof]
fn io_promises_cover_every_emission() {
    let before = any_state();
    if let Ok((after, emit)) = io_step(before, any_event()) {
        let emitted = match emit {
            IoEmit::Nothing => 0u32,
            IoEmit::Submit | IoEmit::Cancel => 1,
        };
        assert!(before.promises() >= after.promises() + emitted);
    }
}

fn some_io() -> IoId {
    let mut arena: Arena<kind::Io, ()> = Arena::new(crate::InstanceId::new(1), 1);
    let id = arena.insert(());
    kani::assume(id.is_ok());
    id.unwrap()
}

fn any_seats() -> (Seats, u64) {
    let mut seats = Seats::new(kani::any());
    let queued: u64 = kani::any();
    // 予約できた状態から始める。空の勘定から予約できる数は、キューの数と合わせて上限以下である。
    kani::assume(seats.promise(queued, kani::any()).is_ok());
    (seats, queued)
}

/// 席の勘定は回り込まず、キューと予約の合計は上限を超えない (台帳 FX-1)。断られた予約は何も変えない。
/// 上限、キューの数、予約の数は全ての値。勘定は整数だけなので、確保の成否を前提にしない。
#[kani::proof]
fn seats_never_promise_beyond_the_limit() {
    let (mut seats, queued) = any_seats();
    assert!(seats.within_limit(queued));
    let (limit, held) = (seats.limit(), seats.promised());
    let count: u32 = kani::any();
    let fits = u128::from(queued) + u128::from(held) + u128::from(count) <= u128::from(limit);
    assert!(seats.room_for(queued, count).is_ok() == fits);
    match seats.promise(queued, count) {
        Ok(()) => assert!(fits && u64::from(seats.promised()) == u64::from(held) + u64::from(count)),
        Err(error) => assert!(!fits && matches!(error, Error::Exhausted(_)) && seats.promised() == held),
    }
    assert!(seats.within_limit(queued) && seats.limit() == limit);
}

/// 予約を返すと、返した数だけ減る。持っている数より多くは返せず、そのときは何も変えない。
/// 予約した `Effect` を出すとき (一席を返してキューが一つ増える) も、合計は上限を超えない (台帳 FX-1)。
#[kani::proof]
fn seats_return_only_what_was_promised() {
    let (mut seats, queued) = any_seats();
    let held = seats.promised();
    let back: u32 = kani::any();
    match seats.forgo(back) {
        Ok(()) => assert!(back <= held && seats.promised() == held - back),
        Err(error) => assert!(back > held && matches!(error, Error::Broken(_)) && seats.promised() == held),
    }
    assert!(seats.within_limit(queued));
    // 発行: 一席を返せたなら、キューが一つ増えても上限に収まる。
    let before = seats.promised();
    if seats.forgo(1).is_ok() {
        assert!(before >= 1 && queued < u64::MAX && seats.within_limit(queued + 1));
    }
}

/// 予約した `Effect` は必ず出せる。予約していない `Effect` は出せない (台帳 FX-1)。
#[kani::proof]
#[kani::unwind(4)]
fn effects_emit_exactly_what_was_promised() {
    let mut effects = match Effects::new(2) {
        Ok(effects) => effects,
        Err(_) => return,
    };
    let count: u32 = kani::any();
    kani::assume(count <= 2);
    assert!(effects.promise(count).is_ok());
    let io = some_io();
    let mut sent = 0u32;
    while sent < count {
        assert!(effects.emit(Effect::IoSubmit { io }).is_ok());
        sent += 1;
        assert!(effects.within_limit());
    }
    assert!(effects.emit(Effect::IoSubmit { io }).is_err());
    assert!(effects.promised() == 0);
    // 出した Effect は取り出されるまで席を使い続ける。
    assert!(effects.promise(1).is_ok() == (count < 2));
}

/// 取り除いた資源の ID は、スロットが再利用されても二度と解決しない (台帳 ID-1)。
/// 最初の世代は任意 (使い切る直前まで)。
#[kani::proof]
#[kani::unwind(3)]
fn a_removed_id_never_resolves_again() {
    let first: u32 = kani::any();
    kani::assume(first != 0 && first != u32::MAX);
    let mut arena: Arena<kind::Io, u8> = Arena::starting_at(crate::InstanceId::new(1), 1, NonZeroU32::new(first).unwrap());
    let made = arena.insert(1);
    kani::assume(made.is_ok());
    let old = made.unwrap();
    assert!(arena.remove(old) == Some(1));
    assert!(arena.get(old).is_none());
    let made = arena.insert(2);
    kani::assume(made.is_ok());
    let new = made.unwrap();
    assert!(new.slot() == old.slot() && new.generation() == first + 1);
    assert!(arena.get(old).is_none());
    assert!(arena.remove(old).is_none());
    assert!(arena.get(new) == Some(&2));
}

/// 世代を使い切ったスロットは退役し、古い ID と同じ値の ID は二度と作られない (台帳 ID-1)。
#[kani::proof]
#[kani::unwind(3)]
fn an_exhausted_slot_is_retired() {
    let mut arena: Arena<kind::Io, u8> = Arena::starting_at(crate::InstanceId::new(1), 1, NonZeroU32::MAX);
    let made = arena.insert(1);
    kani::assume(made.is_ok());
    let old = made.unwrap();
    assert!(arena.remove(old) == Some(1));
    assert!(arena.get(old).is_none());
    let made = arena.insert(2);
    kani::assume(made.is_ok());
    let new = made.unwrap();
    assert!(new.slot() != old.slot());
    assert!(arena.get(old).is_none());
    assert!(arena.get(new) == Some(&2));
}

/// 別の実行インスタンスが発行した ID は、スロットと世代が同じでも資源を指さない (台帳 ID-1)。
#[kani::proof]
#[kani::unwind(3)]
fn an_id_from_another_issuer_never_resolves() {
    let (here, there): (u64, u64) = (kani::any(), kani::any());
    kani::assume(here != there);
    let mut mine: Arena<kind::Io, u8> = Arena::new(crate::InstanceId::new(here), 1);
    let mut theirs: Arena<kind::Io, u8> = Arena::new(crate::InstanceId::new(there), 1);
    let (made, other) = (mine.insert(1), theirs.insert(2));
    kani::assume(made.is_ok() && other.is_ok());
    let (own, foreign) = (made.unwrap(), other.unwrap());
    assert!(own.slot() == foreign.slot() && own.generation() == foreign.generation());
    assert!(own != foreign);
    assert!(mine.get(foreign).is_none());
    assert!(mine.get_mut(foreign).is_none());
    assert!(mine.remove(foreign).is_none());
    assert!(mine.get(own) == Some(&1));
}
