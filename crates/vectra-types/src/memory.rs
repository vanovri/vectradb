use crate::error::{Result, VectraError};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Standard cache-line alignment (64 bytes) for all columnar buffers.
pub const CACHE_LINE_ALIGN: usize = 64;

/// Trait for tracking and bounding heap memory consumption across all VectraDB operators.
pub trait MemoryPool: Send + Sync + fmt::Debug {
    /// Attempt to reserve `bytes` from this pool, returning an RAII `MemoryReservation`.
    fn try_reserve(self: &Arc<Self>, bytes: usize) -> Result<MemoryReservation>
    where
        Self: Sized + 'static,
    {
        let pool: Arc<dyn MemoryPool> = Arc::clone(self) as Arc<dyn MemoryPool>;
        MemoryReservation::try_new(pool, bytes)
    }

    /// Low-level atomic reservation increment. Prefer `MemoryReservation`.
    fn try_acquire(&self, bytes: usize) -> Result<()>;

    /// Low-level release counterpart to `try_acquire`.
    fn release(&self, bytes: usize);

    /// Current live reserved bytes.
    fn current_bytes(&self) -> usize;

    /// High-water mark of reserved bytes since pool creation.
    fn peak_bytes(&self) -> usize;

    /// Hard upper bound in bytes.
    fn limit_bytes(&self) -> usize;
}

/// Extension trait for reserving memory from an `Arc<dyn MemoryPool>`.
pub trait MemoryPoolExt {
    fn reserve(&self, bytes: usize) -> Result<MemoryReservation>;
}

impl MemoryPoolExt for Arc<dyn MemoryPool> {
    #[inline]
    fn reserve(&self, bytes: usize) -> Result<MemoryReservation> {
        MemoryReservation::try_new(Arc::clone(self), bytes)
    }
}

/// Lock-free atomic bounded memory pool enforcing a strict byte ceiling.
#[derive(Debug)]
pub struct BoundedMemoryPool {
    current: AtomicUsize,
    peak: AtomicUsize,
    limit: usize,
}

impl BoundedMemoryPool {
    pub fn new(limit_bytes: usize) -> Arc<Self> {
        Arc::new(Self {
            current: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            limit: limit_bytes,
        })
    }

    pub fn unbounded() -> Arc<Self> {
        Self::new(usize::MAX)
    }

    pub fn as_dyn(self: &Arc<Self>) -> Arc<dyn MemoryPool> {
        Arc::clone(self) as Arc<dyn MemoryPool>
    }

    /// Explicitly assert zero live bytes remain reserved (usable in release mode tests).
    pub fn assert_no_leaks(&self) {
        let leaked = self.current.load(Ordering::Acquire);
        assert_eq!(
            leaked,
            0,
            "BoundedMemoryPool leaked {leaked} bytes (peak was {})",
            self.peak.load(Ordering::Acquire)
        );
    }
}

impl Drop for BoundedMemoryPool {
    fn drop(&mut self) {
        let leaked = *self.current.get_mut();
        debug_assert_eq!(
            leaked, 0,
            "BoundedMemoryPool dropped with {leaked} bytes still reserved"
        );
    }
}

impl MemoryPool for BoundedMemoryPool {
    fn try_acquire(&self, bytes: usize) -> Result<()> {
        if bytes == 0 {
            return Ok(());
        }
        let mut cur = self.current.load(Ordering::Relaxed);
        loop {
            let next = match cur.checked_add(bytes) {
                Some(n) if n <= self.limit => n,
                _ => {
                    return Err(VectraError::OutOfMemory {
                        requested: bytes,
                        available: self.limit.saturating_sub(cur),
                        limit: self.limit,
                    });
                }
            };
            match self
                .current
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => {
                    self.peak.fetch_max(next, Ordering::AcqRel);
                    return Ok(());
                }
                Err(observed) => cur = observed,
            }
        }
    }

    fn release(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let prev = self.current.fetch_sub(bytes, Ordering::AcqRel);
        debug_assert!(
            prev >= bytes,
            "MemoryPool underflow: tried to release {bytes} with only {prev} reserved"
        );
    }

    #[inline]
    fn current_bytes(&self) -> usize {
        self.current.load(Ordering::Acquire)
    }

    #[inline]
    fn peak_bytes(&self) -> usize {
        self.peak.load(Ordering::Acquire)
    }

    #[inline]
    fn limit_bytes(&self) -> usize {
        self.limit
    }
}

/// RAII token representing `bytes` reserved from a `MemoryPool`.
/// Guarantees exact release upon `Drop`.
#[derive(Debug)]
pub struct MemoryReservation {
    pool: Arc<dyn MemoryPool>,
    bytes: usize,
}

impl MemoryReservation {
    pub fn try_new(pool: Arc<dyn MemoryPool>, bytes: usize) -> Result<Self> {
        pool.try_acquire(bytes)?;
        Ok(Self { pool, bytes })
    }

    pub fn empty(pool: Arc<dyn MemoryPool>) -> Self {
        Self { pool, bytes: 0 }
    }

    #[inline]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    #[inline]
    pub fn pool(&self) -> &Arc<dyn MemoryPool> {
        &self.pool
    }

    pub fn try_grow(&mut self, delta: usize) -> Result<()> {
        if delta == 0 {
            return Ok(());
        }
        self.pool.try_acquire(delta)?;
        self.bytes = self
            .bytes
            .checked_add(delta)
            .ok_or_else(|| VectraError::Internal("reservation byte counter overflow".into()))?;
        Ok(())
    }

    pub fn shrink(&mut self, delta: usize) {
        let actual = delta.min(self.bytes);
        if actual > 0 {
            self.bytes -= actual;
            self.pool.release(actual);
        }
    }

    pub fn resize(&mut self, new_bytes: usize) -> Result<()> {
        if new_bytes > self.bytes {
            self.try_grow(new_bytes - self.bytes)
        } else {
            self.shrink(self.bytes - new_bytes);
            Ok(())
        }
    }

    /// Splits `take` bytes off this reservation into a new independent `MemoryReservation`
    /// with zero net change to the underlying pool.
    pub fn split(&mut self, take: usize) -> Result<Self> {
        if take > self.bytes {
            return Err(VectraError::Internal(format!(
                "cannot split {take} bytes from reservation holding {}",
                self.bytes
            )));
        }
        self.bytes -= take;
        Ok(Self {
            pool: Arc::clone(&self.pool),
            bytes: take,
        })
    }

    /// Absorbs another reservation from the same pool without double-releasing.
    pub fn absorb(&mut self, mut other: Self) {
        debug_assert!(Arc::ptr_eq(&self.pool, &other.pool));
        self.bytes += other.bytes;
        other.bytes = 0;
    }

    /// Releases all currently held bytes back to the pool.
    pub fn clear(&mut self) {
        if self.bytes > 0 {
            let b = self.bytes;
            self.bytes = 0;
            self.pool.release(b);
        }
    }
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Quarantined unsafe submodule implementing 64-byte cache-line aligned buffers
/// tracked by `MemoryReservation` and shared via `Arc` for zero-copy slicing.
mod aligned_buf {
    use super::{MemoryPool, MemoryReservation, CACHE_LINE_ALIGN};
    use crate::error::{Result, VectraError};
    use std::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};
    use std::ptr::NonNull;
    use std::slice;
    use std::sync::Arc;

    #[repr(align(64))]
    struct Align64Dummy;

    #[inline]
    fn dangling_aligned64() -> NonNull<u8> {
        NonNull::<Align64Dummy>::dangling().cast::<u8>()
    }

    #[derive(Debug)]
    struct RawAlignedBuf {
        ptr: NonNull<u8>,
        cap: usize,
        _reservation: MemoryReservation,
    }

    // SAFETY: `RawAlignedBuf` owns its heap allocation exclusively (shared only immutably
    // through `Arc` once frozen, or uniquely via `AlignedBufferMut`), and the backing
    // bytes are plain POD bytes (`u8`) with no interior mutability or thread-local state.
    unsafe impl Send for RawAlignedBuf {}
    // SAFETY: Immutable shared reads across threads on frozen `RawAlignedBuf` are data-race free.
    unsafe impl Sync for RawAlignedBuf {}

    impl RawAlignedBuf {
        fn new_zeroed(cap: usize, pool: Arc<dyn MemoryPool>) -> Result<Self> {
            if cap == 0 {
                return Ok(Self {
                    ptr: dangling_aligned64(),
                    cap: 0,
                    _reservation: MemoryReservation::empty(pool),
                });
            }
            let layout = Layout::from_size_align(cap, CACHE_LINE_ALIGN)
                .map_err(|e| VectraError::Internal(format!("invalid buffer layout: {e}")))?;
            let reservation = MemoryReservation::try_new(pool, cap)?;
            // SAFETY: `cap > 0` and `layout` was verified by `Layout::from_size_align` with
            // alignment 64 (power of two). Reservation succeeded before heap allocation.
            let raw = unsafe { alloc_zeroed(layout) };
            let ptr = match NonNull::new(raw) {
                Some(p) => p,
                None => {
                    drop(reservation);
                    handle_alloc_error(layout);
                }
            };
            Ok(Self {
                ptr,
                cap,
                _reservation: reservation,
            })
        }

        #[inline]
        fn as_slice(&self, offset: usize, len: usize) -> &[u8] {
            assert!(
                offset <= self.cap && len <= self.cap - offset,
                "AlignedBuffer slice out of bounds: offset={offset}, len={len}, cap={}",
                self.cap
            );
            if len == 0 {
                return &[];
            }
            // SAFETY: `offset + len <= self.cap`, `self.ptr` is valid for `self.cap` initialized
            // bytes (allocated via `alloc_zeroed`), and immutable borrow prevents concurrent writes.
            unsafe { slice::from_raw_parts(self.ptr.as_ptr().add(offset), len) }
        }

        #[inline]
        fn as_mut_slice(&mut self, len: usize) -> &mut [u8] {
            assert!(
                len <= self.cap,
                "AlignedBufferMut slice out of bounds: len={len}, cap={}",
                self.cap
            );
            if len == 0 {
                return &mut [];
            }
            // SAFETY: `&mut self` guarantees exclusive access, `len <= self.cap`, and all `self.cap`
            // bytes were zero-initialized upon allocation.
            unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), len) }
        }
    }

    impl Drop for RawAlignedBuf {
        fn drop(&mut self) {
            if self.cap > 0 {
                let layout = Layout::from_size_align(self.cap, CACHE_LINE_ALIGN)
                    .expect("validated layout at allocation");
                // SAFETY: `self.ptr` was allocated with `alloc_zeroed(layout)` and has not been
                // deallocated yet; `Drop` runs at most once when the final owner drops.
                unsafe {
                    dealloc(self.ptr.as_ptr(), layout);
                }
            }
        }
    }

    /// Immutable, 64-byte cache-line aligned, reference-counted buffer supporting O(1) slicing.
    #[derive(Debug, Clone)]
    pub struct AlignedBuffer {
        raw: Arc<RawAlignedBuf>,
        offset: usize,
        len: usize,
    }

    impl AlignedBuffer {
        pub fn empty(pool: &Arc<dyn MemoryPool>) -> Self {
            let raw = RawAlignedBuf::new_zeroed(0, Arc::clone(pool))
                .expect("zero-byte reservation cannot fail");
            Self {
                raw: Arc::new(raw),
                offset: 0,
                len: 0,
            }
        }

        pub fn from_slice(data: &[u8], pool: &Arc<dyn MemoryPool>) -> Result<Self> {
            let mut buf = AlignedBufferMut::with_capacity(data.len(), pool)?;
            buf.extend_from_slice(data)?;
            Ok(buf.freeze())
        }

        pub fn from_typed_slice<T: Copy>(data: &[T], pool: &Arc<dyn MemoryPool>) -> Result<Self> {
            let byte_len = std::mem::size_of_val(data);
            let mut buf = AlignedBufferMut::with_capacity(byte_len, pool)?;
            buf.extend_from_typed_slice(data)?;
            Ok(buf.freeze())
        }

        #[inline]
        pub fn len(&self) -> usize {
            self.len
        }

        #[inline]
        pub fn is_empty(&self) -> bool {
            self.len == 0
        }

        #[inline]
        pub fn capacity(&self) -> usize {
            self.raw.cap
        }

        #[inline]
        pub fn as_ptr(&self) -> *const u8 {
            if self.len == 0 {
                dangling_aligned64().as_ptr()
            } else {
                // SAFETY: `self.offset < self.raw.cap` because `self.len > 0` and
                // `self.offset + self.len <= self.raw.cap`.
                unsafe { self.raw.ptr.as_ptr().add(self.offset) }
            }
        }

        #[inline]
        pub fn as_slice(&self) -> &[u8] {
            self.raw.as_slice(self.offset, self.len)
        }

        /// Zero-copy O(1) sub-slice sharing the same backing allocation.
        pub fn slice(&self, start: usize, len: usize) -> Self {
            assert!(
                start <= self.len && len <= self.len - start,
                "AlignedBuffer::slice({start}, {len}) out of bounds for len {}",
                self.len
            );
            Self {
                raw: Arc::clone(&self.raw),
                offset: self.offset + start,
                len,
            }
        }

        /// Interprets the buffer bytes as a typed slice `&[T]`.
        /// Panics if alignment or byte-length divisibility is violated.
        #[inline]
        pub fn as_typed_slice<T: Copy>(&self) -> &[T] {
            let align = std::mem::align_of::<T>();
            let size = std::mem::size_of::<T>();
            assert!(
                align <= CACHE_LINE_ALIGN,
                "type alignment {align} exceeds buffer alignment {CACHE_LINE_ALIGN}"
            );
            assert!(size > 0, "zero-sized types not supported in AlignedBuffer");
            assert_eq!(
                self.len % size,
                0,
                "buffer len {} not a multiple of size_of::<T>() = {size}",
                self.len
            );
            if self.len == 0 {
                return &[];
            }
            let ptr = self.as_ptr();
            assert_eq!(
                (ptr as usize) % align,
                0,
                "sliced buffer pointer {:p} not aligned to {align}",
                ptr
            );
            let count = self.len / size;
            // SAFETY: `ptr` is properly aligned to `align`, valid for `self.len = count * size`
            // initialized bytes, `T: Copy` has no drop glue, and caller upholds bit-validity
            // by only invoking on POD scalar/ByteView buffers written with matching `T`.
            unsafe { slice::from_raw_parts(ptr.cast::<T>(), count) }
        }
    }

    /// Owned, mutable 64-byte aligned buffer tracked by `MemoryPool`.
    #[derive(Debug)]
    pub struct AlignedBufferMut {
        raw: RawAlignedBuf,
        len: usize,
        pool: Arc<dyn MemoryPool>,
    }

    impl AlignedBufferMut {
        pub fn with_capacity(cap: usize, pool: &Arc<dyn MemoryPool>) -> Result<Self> {
            let raw = RawAlignedBuf::new_zeroed(cap, Arc::clone(pool))?;
            Ok(Self {
                raw,
                len: 0,
                pool: Arc::clone(pool),
            })
        }

        pub fn zeroed(len: usize, pool: &Arc<dyn MemoryPool>) -> Result<Self> {
            let raw = RawAlignedBuf::new_zeroed(len, Arc::clone(pool))?;
            Ok(Self {
                raw,
                len,
                pool: Arc::clone(pool),
            })
        }

        #[inline]
        pub fn len(&self) -> usize {
            self.len
        }

        #[inline]
        pub fn is_empty(&self) -> bool {
            self.len == 0
        }

        #[inline]
        pub fn capacity(&self) -> usize {
            self.raw.cap
        }

        pub fn reserve(&mut self, additional: usize) -> Result<()> {
            let required = self
                .len
                .checked_add(additional)
                .ok_or_else(|| VectraError::Internal("buffer capacity overflow".into()))?;
            if required <= self.raw.cap {
                return Ok(());
            }
            let mut new_cap = self.raw.cap.max(CACHE_LINE_ALIGN);
            while new_cap < required {
                new_cap = new_cap.saturating_mul(2).max(required);
            }
            // Round up to multiple of 64 bytes for clean cache-line sizing.
            let rounded = new_cap
                .checked_add(CACHE_LINE_ALIGN - 1)
                .map(|v| v & !(CACHE_LINE_ALIGN - 1))
                .unwrap_or(required);
            let target_cap = if rounded >= required {
                rounded
            } else {
                required
            };

            // First try allocating exact rounded capacity; if tight memory pool rejects over-allocation,
            // fall back to exact `required` capacity!
            let mut new_raw = match RawAlignedBuf::new_zeroed(target_cap, Arc::clone(&self.pool)) {
                Ok(r) => r,
                Err(VectraError::OutOfMemory { .. }) if target_cap > required => {
                    RawAlignedBuf::new_zeroed(required, Arc::clone(&self.pool))?
                }
                Err(e) => return Err(e),
            };
            if self.len > 0 {
                new_raw
                    .as_mut_slice(self.len)
                    .copy_from_slice(self.raw.as_slice(0, self.len));
            }
            self.raw = new_raw;
            Ok(())
        }

        pub fn resize_zeroed(&mut self, new_len: usize) -> Result<()> {
            if new_len > self.len {
                self.reserve(new_len - self.len)?;
                let dst = &mut self.raw.as_mut_slice(new_len)[self.len..new_len];
                dst.fill(0);
            }
            self.len = new_len;
            Ok(())
        }

        pub fn extend_from_slice(&mut self, bytes: &[u8]) -> Result<()> {
            if bytes.is_empty() {
                return Ok(());
            }
            self.reserve(bytes.len())?;
            let start = self.len;
            let end = start + bytes.len();
            self.raw.as_mut_slice(end)[start..end].copy_from_slice(bytes);
            self.len = end;
            Ok(())
        }

        pub fn push_typed<T: Copy>(&mut self, value: T) -> Result<()> {
            let size = std::mem::size_of::<T>();
            assert!(std::mem::align_of::<T>() <= CACHE_LINE_ALIGN);
            assert_eq!(self.len % std::mem::align_of::<T>(), 0);
            self.reserve(size)?;
            let start = self.len;
            // SAFETY: `self.reserve(size)` ensured `start + size <= self.raw.cap`,
            // `self.raw.ptr` is 64-byte aligned and `start` is aligned to `align_of::<T>()`.
            unsafe {
                let dst = self.raw.ptr.as_ptr().add(start).cast::<T>();
                dst.write(value);
            }
            self.len += size;
            Ok(())
        }

        pub fn extend_from_typed_slice<T: Copy>(&mut self, values: &[T]) -> Result<()> {
            if values.is_empty() {
                return Ok(());
            }
            let byte_len = std::mem::size_of_val(values);
            assert!(std::mem::align_of::<T>() <= CACHE_LINE_ALIGN);
            assert_eq!(self.len % std::mem::align_of::<T>(), 0);
            self.reserve(byte_len)?;
            let start = self.len;
            // SAFETY: Destination has at least `values.len()` aligned `T` slots within `self.raw.cap`,
            // source and destination cannot overlap because `self.raw` is uniquely owned.
            unsafe {
                let dst = self.raw.ptr.as_ptr().add(start).cast::<T>();
                std::ptr::copy_nonoverlapping(values.as_ptr(), dst, values.len());
            }
            self.len += byte_len;
            Ok(())
        }

        #[inline]
        pub fn as_slice(&self) -> &[u8] {
            self.raw.as_slice(0, self.len)
        }

        #[inline]
        pub fn as_mut_slice(&mut self) -> &mut [u8] {
            self.raw.as_mut_slice(self.len)
        }

        #[inline]
        pub fn as_typed_mut_slice<T: Copy>(&mut self) -> &mut [T] {
            let align = std::mem::align_of::<T>();
            let size = std::mem::size_of::<T>();
            assert!(align <= CACHE_LINE_ALIGN && size > 0);
            assert_eq!(self.len % size, 0);
            if self.len == 0 {
                return &mut [];
            }
            let count = self.len / size;
            // SAFETY: `self.raw.ptr` is 64-byte aligned (`>= align`), valid for `self.len` bytes,
            // and `&mut self` guarantees exclusive mutable access.
            unsafe { slice::from_raw_parts_mut(self.raw.ptr.as_ptr().cast::<T>(), count) }
        }

        #[inline]
        pub fn freeze(self) -> AlignedBuffer {
            AlignedBuffer {
                raw: Arc::new(self.raw),
                offset: 0,
                len: self.len,
            }
        }
    }
}

pub use aligned_buf::{AlignedBuffer, AlignedBufferMut};
