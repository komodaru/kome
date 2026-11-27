//! アドレスからマッピングを引く二分探索。
//!
//! 表は先頭アドレスの昇順で、重ならない (台帳 MEM-2)。その前提のもとで、表の長さによらず、
//! 引いた結果が「そのアドレスを含むただ一つのマッピング」であることを Verus が証明する
//! (`docs/verus.md`)。Verus の仕様は `cfg_attr(verus_keep_ghost, ..)` で書くので、普通の
//! コンパイルでは消え、依存は増えない。

#[cfg(verus_keep_ghost)]
use verus_builtin_macros::{verus, verus_spec};
#[cfg(verus_keep_ghost)]
use vstd::prelude::*;

use crate::mem::Mapping;

#[cfg(verus_keep_ghost)]
verus! {

/// Verus から見た `Mapping`。中身は見せず、先頭と末尾だけを下の二つの関数で表す。
#[verifier::external_type_specification]
#[verifier::external_body]
pub struct ExMapping(Mapping);

/// マッピングの先頭のアドレス。
pub uninterp spec fn start_of(mapping: Mapping) -> u64;

/// マッピングの最後のバイトのアドレス。
pub uninterp spec fn last_of(mapping: Mapping) -> u64;

/// アドレスがマッピングの中にあるか。
pub open spec fn holds(mapping: Mapping, addr: u64) -> bool {
    start_of(mapping) <= addr <= last_of(mapping)
}

/// 表が昇順で、重ならない: どのマッピングも空でなく、前のものの末尾は後のものの先頭より前にある。
pub open spec fn ordered(table: Seq<Mapping>) -> bool {
    &&& forall|i: int| 0 <= i < table.len() ==> start_of(#[trigger] table[i]) <= last_of(table[i])
    &&& forall|i: int, j: int|
        0 <= i < j < table.len() ==> last_of(#[trigger] table[i]) < start_of(#[trigger] table[j])
}

} // verus!

/// `addr` 以降に終わる最初のマッピングの位置 (無ければ表の長さ)。
#[cfg_attr(verus_keep_ghost, verus_spec(at =>
    requires
        ordered(table@),
    ensures
        at <= table@.len(),
        forall|i: int| 0 <= i < at ==> last_of(#[trigger] table@[i]) < addr,
        forall|i: int| at <= i < table@.len() ==> last_of(#[trigger] table@[i]) >= addr,
))]
pub(crate) fn first_ending_at_or_after(table: &[Mapping], addr: u64) -> usize {
    let mut low = 0usize;
    let mut high = table.len();
    #[cfg_attr(verus_keep_ghost, verus_spec(
        invariant
            low <= high <= table@.len(),
            ordered(table@),
            forall|i: int| 0 <= i < low ==> last_of(#[trigger] table@[i]) < addr,
            forall|i: int| high <= i < table@.len() ==> last_of(#[trigger] table@[i]) >= addr,
        decreases high - low
    ))]
    while low < high {
        let middle = low.saturating_add(high.saturating_sub(low) / 2);
        match table.get(middle) {
            Some(mapping) if mapping.last() < addr => low = middle.saturating_add(1),
            Some(_) | None => high = middle,
        }
    }
    low
}

/// `addr` を含むマッピングの位置。
#[cfg_attr(verus_keep_ghost, verus_spec(found =>
    requires
        ordered(table@),
    ensures
        match found {
            Some(at) => at < table@.len() && holds(table@[at as int], addr),
            None => forall|i: int| 0 <= i < table@.len() ==> !holds(#[trigger] table@[i], addr),
        },
))]
pub(crate) fn containing(table: &[Mapping], addr: u64) -> Option<usize> {
    let at = first_ending_at_or_after(table, addr);
    match table.get(at) {
        Some(mapping) if mapping.start() <= addr => Some(at),
        Some(_) | None => None,
    }
}

/// Verus が誤りを見つけることを確かめるための、わざと誤った定義。`scripts/ci/verus.sh` だけが有効にする。
#[cfg(verus_broken)]
#[cfg_attr(verus_keep_ghost, verus_spec(found =>
    requires
        ordered(table@),
    ensures
        found.is_some(),
))]
pub(crate) fn verus_broken_lookup(table: &[Mapping], addr: u64) -> Option<usize> {
    containing(table, addr)
}
