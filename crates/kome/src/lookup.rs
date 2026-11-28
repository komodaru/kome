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

/// 表の空いた所へ入れても、表は昇順で重ならない ([`free_at`] が真を返した位置への `map`)。
pub proof fn inserting_into_a_gap_keeps_the_order(table: Seq<Mapping>, at: int, new: Mapping)
    requires
        ordered(table),
        0 <= at <= table.len(),
        start_of(new) <= last_of(new),
        forall|i: int| 0 <= i < at ==> last_of(#[trigger] table[i]) < start_of(new),
        forall|i: int| at <= i < table.len() ==> last_of(new) < start_of(#[trigger] table[i]),
    ensures
        ordered(table.insert(at, new)),
{
    let after = table.insert(at, new);
    assert forall|i: int| 0 <= i < after.len() implies start_of(#[trigger] after[i]) <= last_of(after[i]) by {
        if i > at {
            assert(after[i] == table[i - 1]);
        }
    }
    assert forall|i: int, j: int| 0 <= i < j < after.len() implies last_of(#[trigger] after[i]) < start_of(
        #[trigger] after[j],
    ) by {
        if i > at {
            assert(after[i] == table[i - 1]);
        }
        if j > at {
            assert(after[j] == table[j - 1]);
        }
    }
}

/// 項目を一つ取り除いても、表は昇順で重ならない (`unmap` が範囲に収まる項目を外す)。
pub proof fn removing_keeps_the_order(table: Seq<Mapping>, at: int)
    requires
        ordered(table),
        0 <= at < table.len(),
    ensures
        ordered(table.remove(at)),
{
    let after = table.remove(at);
    assert forall|i: int| 0 <= i < after.len() implies start_of(#[trigger] after[i]) <= last_of(after[i]) by {
        if i >= at {
            assert(after[i] == table[i + 1]);
        }
    }
    assert forall|i: int, j: int| 0 <= i < j < after.len() implies last_of(#[trigger] after[i]) < start_of(
        #[trigger] after[j],
    ) by {
        if i >= at {
            assert(after[i] == table[i + 1]);
        }
        if j >= at {
            assert(after[j] == table[j + 1]);
        }
    }
}

/// 項目を、元の範囲の内側へ縮めても、表は昇順で重ならない (`unmap` と `protect` が端を切る)。
pub proof fn narrowing_keeps_the_order(table: Seq<Mapping>, at: int, new: Mapping)
    requires
        ordered(table),
        0 <= at < table.len(),
        start_of(table[at]) <= start_of(new) <= last_of(new) <= last_of(table[at]),
    ensures
        ordered(table.update(at, new)),
{
    let after = table.update(at, new);
    assert forall|i: int, j: int| 0 <= i < j < after.len() implies last_of(#[trigger] after[i]) < start_of(
        #[trigger] after[j],
    ) by {
        assert(last_of(table[i]) < start_of(table[j]));
    }
}

/// 項目を、間の空いた二つへ分けても、表は昇順で重ならない (範囲が一つのマッピングの内側にある
/// `unmap` と、`protect` の分割)。
pub proof fn splitting_keeps_the_order(table: Seq<Mapping>, at: int, left: Mapping, right: Mapping)
    requires
        ordered(table),
        0 <= at < table.len(),
        start_of(table[at]) <= start_of(left) <= last_of(left),
        last_of(left) < start_of(right),
        start_of(right) <= last_of(right) <= last_of(table[at]),
    ensures
        ordered(table.update(at, left).insert(at + 1, right)),
{
    narrowing_keeps_the_order(table, at, left);
    let narrowed = table.update(at, left);
    assert forall|i: int| 0 <= i < at + 1 implies last_of(#[trigger] narrowed[i]) < start_of(right) by {
        if i < at {
            assert(last_of(table[i]) < start_of(table[at]));
        }
    }
    assert forall|i: int| at + 1 <= i < narrowed.len() implies last_of(right) < start_of(#[trigger] narrowed[i]) by {
        assert(last_of(table[at]) < start_of(table[i]));
    }
    inserting_into_a_gap_keeps_the_order(narrowed, at + 1, right);
}

/// 隣り合う二つの項目を、前の先頭から後ろの末尾までの一つにまとめても、表は昇順で重ならない
/// (`protect` が、続いている項目をまとめる)。
pub proof fn joining_keeps_the_order(table: Seq<Mapping>, at: int, joined: Mapping)
    requires
        ordered(table),
        0 <= at,
        at + 1 < table.len(),
        start_of(joined) == start_of(table[at]),
        last_of(joined) == last_of(table[at + 1]),
    ensures
        ordered(table.update(at, joined).remove(at + 1)),
{
    let after = table.update(at, joined).remove(at + 1);
    assert(last_of(table[at]) < start_of(table[at + 1]));
    assert forall|i: int| 0 <= i < after.len() implies start_of(#[trigger] after[i]) <= last_of(after[i]) by {
        if i > at {
            assert(after[i] == table[i + 1]);
        } else if i < at {
            assert(after[i] == table[i]);
        }
    }
    assert forall|i: int, j: int| 0 <= i < j < after.len() implies last_of(#[trigger] after[i]) < start_of(
        #[trigger] after[j],
    ) by {
        if j < at {
            assert(last_of(table[i]) < start_of(table[j]));
        } else if j == at {
            assert(last_of(table[i]) < start_of(table[at]));
        } else {
            assert(after[j] == table[j + 1]);
            if i < at {
                assert(last_of(table[i]) < start_of(table[j + 1]));
            } else if i == at {
                assert(last_of(table[at + 1]) < start_of(table[j + 1]));
            } else {
                assert(after[i] == table[i + 1]);
                assert(last_of(table[i + 1]) < start_of(table[j + 1]));
            }
        }
    }
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

/// 表の `at` 番目から後ろが、全て `last` より後に始まるか。
///
/// `at` が [`first_ending_at_or_after`] が `start` について返した位置なら、`at` より前は全て `start` より
/// 前に終わっている。だから、これが真なら `[start, last]` を `at` へ入れられ、偽なら `at` 番目の
/// マッピングが `[start, last]` と重なる。
#[cfg_attr(verus_keep_ghost, verus_spec(free =>
    requires
        ordered(table@),
        at <= table@.len(),
    ensures
        free ==> forall|i: int| at <= i < table@.len() ==> last < start_of(#[trigger] table@[i]),
        !free ==> at < table@.len() && start_of(table@[at as int]) <= last,
))]
pub(crate) fn free_at(table: &[Mapping], at: usize, last: u64) -> bool {
    match table.get(at) {
        Some(next) => next.start() > last,
        None => true,
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
