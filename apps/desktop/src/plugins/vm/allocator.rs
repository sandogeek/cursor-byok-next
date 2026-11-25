//! Independent heap ceiling, including the backing allocator's aligned headers.
use rquickjs::allocator::{Allocator, RustAllocator};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct HeapAllocator {
    used: usize,
    limit: usize,
    exhausted: Arc<AtomicBool>,
    inner: RustAllocator,
}
impl HeapAllocator {
    pub fn new(limit: usize) -> (Self, Arc<AtomicBool>) {
        let exhausted = Arc::new(AtomicBool::new(false));
        (
            Self {
                used: 0,
                limit,
                exhausted: Arc::clone(&exhausted),
                inner: RustAllocator,
            },
            exhausted,
        )
    }
    // quickjs-jit 0.14.2 RustAllocator rounds to u64 alignment and adds one aligned usize header.
    fn charged(size: usize) -> Option<usize> {
        let alignment = std::mem::align_of::<u64>();
        size.checked_add(alignment - 1)?
            .checked_div(alignment)?
            .checked_mul(alignment)?
            .checked_add(std::mem::size_of::<usize>().max(alignment))
    }
    fn reserve(&self, size: Option<usize>, old: usize) -> Option<usize> {
        let total = size.and_then(|size| self.used.checked_sub(old)?.checked_add(size));
        if total.is_none_or(|total| total > self.limit) {
            self.exhausted.store(true, Ordering::Release);
            None
        } else {
            total
        }
    }
}
// SAFETY: every pointer is allocated, resized and freed by the same RustAllocator;
// the wrapper only refuses allocations and tracks the allocator's documented layout.
unsafe impl Allocator for HeapAllocator {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        let Some(total) = self.reserve(Self::charged(size), 0) else {
            return std::ptr::null_mut();
        };
        let pointer = self.inner.alloc(size);
        if pointer.is_null() {
            self.exhausted.store(true, Ordering::Release);
        } else {
            self.used = total;
        }
        pointer
    }
    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        let Some(size) = count.checked_mul(size) else {
            self.exhausted.store(true, Ordering::Release);
            return std::ptr::null_mut();
        };
        let pointer = self.alloc(size);
        if !pointer.is_null() {
            // SAFETY: alloc returned writable storage for at least size bytes.
            unsafe {
                pointer.write_bytes(0, size);
            }
        }
        pointer
    }
    unsafe fn dealloc(&mut self, pointer: *mut u8) {
        // SAFETY: QuickJS returns a live pointer from this allocator.
        let old = unsafe { RustAllocator::usable_size(pointer) };
        self.used -= Self::charged(old).expect("live allocation size was checked");
        // SAFETY: the live allocation belongs to this same backing allocator.
        unsafe {
            self.inner.dealloc(pointer);
        }
    }
    unsafe fn realloc(&mut self, pointer: *mut u8, size: usize) -> *mut u8 {
        // SAFETY: QuickJS supplies a live pointer from this allocator.
        let old = unsafe { RustAllocator::usable_size(pointer) };
        let Some(total) = self.reserve(
            Self::charged(size),
            Self::charged(old).expect("live allocation size was checked"),
        ) else {
            return std::ptr::null_mut();
        };
        // SAFETY: same backing allocator, valid old pointer, checked bounded size.
        let resized = unsafe { self.inner.realloc(pointer, size) };
        if resized.is_null() {
            self.exhausted.store(true, Ordering::Release);
        } else {
            self.used = total;
        }
        resized
    }
    unsafe fn usable_size(pointer: *mut u8) -> usize {
        // SAFETY: the caller guarantees a live allocation of this allocator.
        unsafe { RustAllocator::usable_size(pointer) }
    }
}
