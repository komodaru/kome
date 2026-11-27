//! 確保を数えるアロケータ。`tests/noalloc.rs` が大域のアロケータとして使う。
//!
//! 印 ([`enter`] と [`leave`]) の間に、同じスレッドで起きた確保と再確保を数える。
//! 数えるかどうかの印と数はスレッドごとに持つので、並行して走る試験は互いに影響しない。

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

/// システムのアロケータへそのまま渡し、印の間の確保だけを数える。
pub struct Counting;

fn note() {
    if ARMED.get() {
        COUNT.set(COUNT.get().saturating_add(1));
    }
}

// SAFETY: 確保・解放・再確保は全て System へそのまま渡す。数えるための処理はスレッドごとの
// Cell の読み書きだけで、確保も解放もしない (const で初期化するので初回の確保も無い)。
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: 呼び出し側の約束 (layout の大きさが 0 でない) をそのまま System へ渡す。
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: ptr は上の alloc / realloc が System から得たもので、layout も同じ。
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: ptr は System から得たもので、layout と new_size は呼び出し側の約束どおり。
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// 数え始める。
pub fn enter() {
    ARMED.set(true);
}

/// 数えるのをやめる。
pub fn leave() {
    ARMED.set(false);
}

/// このスレッドで、印の間に起きた確保の数。
pub fn count() -> u64 {
    COUNT.get()
}
