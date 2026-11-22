use crate::error::{Result, VectraError};
use crate::memory::{AlignedBuffer, AlignedBufferMut, MemoryPool};
use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

/// Maximum byte length stored completely inline inside `ByteView` without heap indirection.
pub const BYTE_VIEW_INLINE_MAX: usize = 12;

/// Default block size for out-of-line string heap chunks (64 KiB).
pub const DEFAULT_STRING_HEAP_BLOCK: usize = 64 * 1024;

/// Out-of-line descriptor stored when `ByteView::len > 12`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutlineView {
    pub prefix: [u8; 4],
    pub buffer_idx: u32,
    pub offset: u32,
}

/// Union payload for `ByteView` (exact 12 bytes, alignment 4).
/// Both variants initialize all 12 bytes, and bytes `0..4` always hold the string prefix
/// (zero-padded for strings shorter than 4 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub union ByteViewPayload {
    pub inline: [u8; 12],
    pub outline: OutlineView,
}

/// Umbra / Velox "German String" 16-byte cache-resident string view.
///
/// Memory layout (`#[repr(C, align(16))]`, exact size = 16 bytes):
/// - `len <= 12`: `[len: u32, inline: [u8; 12]]` (trailing bytes `len..12` zero-padded)
/// - `len > 12`:  `[len: u32, prefix: [u8; 4], buffer_idx: u32, offset: u32]`
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct ByteView {
    pub len: u32,
    pub payload: ByteViewPayload,
}

// Constitutional static layout assertions.
const _: () = assert!(std::mem::size_of::<ByteView>() == 16);
const _: () = assert!(std::mem::align_of::<ByteView>() == 16);
const _: () = assert!(std::mem::size_of::<ByteViewPayload>() == 12);
const _: () = assert!(std::mem::size_of::<OutlineView>() == 12);

impl Default for ByteView {
    #[inline]
    fn default() -> Self {
        Self::empty()
    }
}

impl ByteView {
    #[inline]
    pub const fn empty() -> Self {
        Self {
            len: 0,
            payload: ByteViewPayload { inline: [0u8; 12] },
        }
    }

    /// Creates an inline `ByteView` directly from `bytes` (`bytes.len() <= 12`).
    #[inline]
    pub fn new_inline(bytes: &[u8]) -> Self {
        assert!(
            bytes.len() <= BYTE_VIEW_INLINE_MAX,
            "inline ByteView max is {BYTE_VIEW_INLINE_MAX}, got {}",
            bytes.len()
        );
        let mut inline = [0u8; 12];
        inline[..bytes.len()].copy_from_slice(bytes);
        Self {
            len: bytes.len() as u32,
            payload: ByteViewPayload { inline },
        }
    }

    /// Creates an out-of-line `ByteView` referencing `(buffer_idx, offset)` in a string heap.
    #[inline]
    pub fn new_outline(bytes: &[u8], buffer_idx: u32, offset: u32) -> Self {
        assert!(
            bytes.len() > BYTE_VIEW_INLINE_MAX,
            "outline ByteView requires len > {BYTE_VIEW_INLINE_MAX}"
        );
        let prefix = [bytes[0], bytes[1], bytes[2], bytes[3]];
        Self {
            len: u32::try_from(bytes.len()).expect("string exceeds u32::MAX bytes"),
            payload: ByteViewPayload {
                outline: OutlineView {
                    prefix,
                    buffer_idx,
                    offset,
                },
            },
        }
    }

    #[inline]
    pub const fn len(&self) -> usize {
        self.len as usize
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub const fn is_inline(&self) -> bool {
        (self.len as usize) <= BYTE_VIEW_INLINE_MAX
    }

    /// Returns the 12-byte zero-padded inline array. Panics if `!self.is_inline()`.
    #[inline]
    pub fn inline_raw(&self) -> [u8; 12] {
        debug_assert!(self.is_inline());
        // SAFETY: `self.is_inline()` guarantees `payload` was initialized via the `inline` variant.
        unsafe { self.payload.inline }
    }

    /// Returns the inline byte slice `&[u8]` (length `self.len`). Panics if `!self.is_inline()`.
    #[inline]
    pub fn inline_slice(&self) -> &[u8] {
        debug_assert!(self.is_inline());
        // SAFETY: `self.is_inline()` guarantees `payload.inline` is active and `self.len <= 12`.
        unsafe { &self.payload.inline[..self.len as usize] }
    }

    /// Returns the outline descriptor. Panics if `self.is_inline()`.
    #[inline]
    pub fn outline(&self) -> OutlineView {
        debug_assert!(!self.is_inline());
        // SAFETY: `!self.is_inline()` guarantees `payload` was initialized via the `outline` variant.
        unsafe { self.payload.outline }
    }

    /// Returns the first 4 bytes (zero-padded if `len < 4`) as a big-endian `u32`
    /// for single-instruction lexicographical rejection without touching the heap.
    #[inline]
    pub fn prefix_u32_be(&self) -> u32 {
        // SAFETY: In `#[repr(C)] ByteViewPayload`, bytes `0..4` of `inline: [u8; 12]` and
        // `outline.prefix: [u8; 4]` occupy identical offset 0..4 and are always fully initialized.
        let p = unsafe { self.payload.outline.prefix };
        u32::from_be_bytes(p)
    }

    /// Returns the raw 16 bytes of this `ByteView` (used by M10 ByteView X-Ray inspector).
    #[inline]
    pub fn raw_bytes(&self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[0..4].copy_from_slice(&self.len.to_le_bytes());
        if self.is_inline() {
            out[4..16].copy_from_slice(&self.inline_raw());
        } else {
            let o = self.outline();
            out[4..8].copy_from_slice(&o.prefix);
            out[8..12].copy_from_slice(&o.buffer_idx.to_le_bytes());
            out[12..16].copy_from_slice(&o.offset.to_le_bytes());
        }
        out
    }

    /// Resolves this `ByteView` to its full `&[u8]` slice using the associated `string_heap`.
    #[inline]
    pub fn as_bytes<'a>(&'a self, string_heap: &'a [AlignedBuffer]) -> &'a [u8] {
        if self.is_inline() {
            self.inline_slice()
        } else {
            let o = self.outline();
            let buf = &string_heap[o.buffer_idx as usize];
            let start = o.offset as usize;
            &buf.as_slice()[start..start + self.len as usize]
        }
    }

    /// Fast German-String equality: rejects unequal lengths and prefixes without touching heap!
    #[inline]
    pub fn eq_fast(a: &Self, heap_a: &[AlignedBuffer], b: &Self, heap_b: &[AlignedBuffer]) -> bool {
        if a.len != b.len {
            return false;
        }
        if a.is_inline() {
            // Both inline and zero-padded: exact 12-byte compare without heap access.
            return a.inline_raw() == b.inline_raw();
        }
        if a.prefix_u32_be() != b.prefix_u32_be() {
            return false;
        }
        // Lengths and first 4 bytes match; compare remaining suffix in heap buffers.
        let sa = a.as_bytes(heap_a);
        let sb = b.as_bytes(heap_b);
        sa[4..] == sb[4..]
    }

    /// Fast German-String lexicographical comparison:
    /// Compares big-endian 4-byte prefixes in a single integer compare before ever reading heap.
    #[inline]
    pub fn cmp_fast(
        a: &Self,
        heap_a: &[AlignedBuffer],
        b: &Self,
        heap_b: &[AlignedBuffer],
    ) -> Ordering {
        let pa = a.prefix_u32_be();
        let pb = b.prefix_u32_be();
        if pa != pb {
            return pa.cmp(&pb);
        }
        // Prefixes matched; fast path if both are inline.
        if a.is_inline() && b.is_inline() {
            return a.inline_slice().cmp(b.inline_slice());
        }
        a.as_bytes(heap_a).cmp(b.as_bytes(heap_b))
    }
}

impl fmt::Debug for ByteView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_inline() {
            f.debug_struct("ByteView::Inline")
                .field("len", &self.len)
                .field("bytes", &self.inline_slice())
                .finish()
        } else {
            let o = self.outline();
            f.debug_struct("ByteView::Outline")
                .field("len", &self.len)
                .field("prefix", &o.prefix)
                .field("buffer_idx", &o.buffer_idx)
                .field("offset", &o.offset)
                .finish()
        }
    }
}

/// Chunked arena builder for out-of-line strings referenced by `ByteView`s.
/// Allocates 64-byte aligned blocks tracked by `MemoryPool`.
#[derive(Debug)]
pub struct StringHeapBuilder {
    pool: Arc<dyn MemoryPool>,
    block_size: usize,
    frozen: Vec<AlignedBuffer>,
    active: Option<AlignedBufferMut>,
}

impl StringHeapBuilder {
    pub fn new(pool: &Arc<dyn MemoryPool>) -> Self {
        Self::with_block_size(pool, DEFAULT_STRING_HEAP_BLOCK)
    }

    pub fn with_block_size(pool: &Arc<dyn MemoryPool>, block_size: usize) -> Self {
        Self {
            pool: Arc::clone(pool),
            block_size: block_size.max(64),
            frozen: Vec::new(),
            active: None,
        }
    }

    /// Appends `bytes` and returns a `ByteView` (stored inline when `len <= 12`).
    pub fn append(&mut self, bytes: &[u8]) -> Result<ByteView> {
        if bytes.len() <= BYTE_VIEW_INLINE_MAX {
            return Ok(ByteView::new_inline(bytes));
        }
        if bytes.len() > u32::MAX as usize {
            return Err(VectraError::ExecutionError(
                "string exceeds 4 GiB ByteView limit".into(),
            ));
        }
        let need_new_block = match &self.active {
            None => true,
            Some(buf) => buf.len().saturating_add(bytes.len()) > buf.capacity(),
        };
        if need_new_block {
            if let Some(prev) = self.active.take() {
                self.frozen.push(prev.freeze());
            }
            let cap = self.block_size.max(bytes.len());
            let buf = match AlignedBufferMut::with_capacity(cap, &self.pool) {
                Ok(b) => b,
                Err(VectraError::OutOfMemory { .. }) if cap > bytes.len() => {
                    AlignedBufferMut::with_capacity(bytes.len(), &self.pool)?
                }
                Err(e) => return Err(e),
            };
            self.active = Some(buf);
        }
        let buffer_idx = u32::try_from(self.frozen.len())
            .map_err(|_| VectraError::ExecutionError("string heap block count overflow".into()))?;
        let active = self.active.as_mut().expect("active buffer allocated");
        let offset = u32::try_from(active.len())
            .map_err(|_| VectraError::ExecutionError("string heap offset overflow".into()))?;
        active.extend_from_slice(bytes)?;
        Ok(ByteView::new_outline(bytes, buffer_idx, offset))
    }

    /// Finalizes all heap blocks into an immutable `Vec<AlignedBuffer>`.
    pub fn finish(mut self) -> Vec<AlignedBuffer> {
        if let Some(active) = self.active.take() {
            if !active.is_empty() {
                self.frozen.push(active.freeze());
            }
        }
        self.frozen
    }
}
