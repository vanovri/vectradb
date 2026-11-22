# M01 Design Document: `vectra-types`

## 1. Objectives & Architectural Guarantees
`vectra-types` establishes the foundational memory management, type system, columnar vector representations, Umbra-style German strings (`ByteView`), compact row serialization (`TupleBuffer`), and normalized binary sort-key encoding used by every upper crate in VectraDB.

## 2. Memory Layouts & Safety Invariants

### 2.1 `BoundedMemoryPool` & `MemoryReservation`
- `BoundedMemoryPool` tracks `current: AtomicUsize` and `peak: AtomicUsize` against an immutable `limit: usize`.
- `try_acquire(delta)` uses a lock-free `compare_exchange_weak` loop ensuring `current + delta <= limit` atomically (using `checked_add` to prevent overflow). `peak` is monotonically updated via `fetch_max`.
- `MemoryReservation` is an RAII token holding `bytes: usize` and `pool: Arc<dyn MemoryPool>`. Every mutation (`try_grow`, `shrink`, `resize`, `split`, `absorb`) preserves the conservation invariant: `sum(live reservations) == pool.current_bytes()`.
- Dropping a `BoundedMemoryPool` verifies `current_bytes() == 0` in debug builds and via explicit `assert_no_leaks()` in release test suites.

### 2.2 `AlignedBuffer` (64-Byte Cache-Line Aligned)
- All non-empty allocations request `Layout::from_size_align(cap, 64)` via `std::alloc::alloc_zeroed`, reserving `cap` bytes from `MemoryPool` **before** calling the global allocator. If `alloc_zeroed` returns null, the reservation drops automatically before invoking `handle_alloc_error`.
- Zero-capacity buffers use a statically aligned 64-byte dummy address (`NonNull::<Align64>::dangling().cast::<u8>()`) with valid provenance so zero-length slices remain 100% Miri-clean without heap allocation.
- `AlignedBuffer` wraps `Arc<RawAlignedBuf>` with `(offset, len)` for O(1) zero-copy slicing.
- Typed slice views (`as_typed_slice::<T>()`) assert `align_of::<T>() <= 64`, `(base + offset) % align_of::<T>() == 0`, and `len % size_of::<T>() == 0`.

### 2.3 Umbra / Velox `ByteView` ("German Strings")
Exact 16-byte layout (`#[repr(C, align(16))]`, statically asserted `size_of::<ByteView>() == 16`):
- Bytes `0..4`: `len: u32`
- If `len <= 12`:
  - Bytes `4..16`: `inline: [u8; 12]` (zero-padded in bytes `len..12` so full 12-byte register equality/prefix comparisons require zero masking!)
- If `len > 12`:
  - Bytes `4..8`: `prefix: [u8; 4]` (first 4 bytes of string)
  - Bytes `8..12`: `buffer_idx: u32` (index into `FlatVector::string_heap`)
  - Bytes `12..16`: `offset: u32` (byte offset inside `string_heap[buffer_idx]`)

Fast-path comparison invariant:
- Equality first checks `a.len == b.len`. For `len <= 12`, compares `a.inline == b.inline` (zero heap reads). For `len > 12`, compares `a.prefix == b.prefix` before dereferencing `string_heap`.
- Ordering first compares `min(4, min(a.len, b.len))` prefix bytes directly in registers, deferring out-of-line heap reads to rare prefix collisions.

### 2.4 `ValidityMask`, `SelectionVector`, `Vector`, `DataChunk`
- `VECTOR_CAPACITY = 2048` (32 × `u64` words per validity bitset).
- `ValidityMask` uses `Option<Arc<[u64]>>` where `None` means all-valid (zero allocation, O(1) branch fast path). Bit iteration uses `trailing_zeros()` and `word &= word - 1`.
- `SelectionVector` supports `Identity(len)` (unallocated fast path) and `Indices(Arc<[u16]>)` with `compose()` for chained late-materialization filters.
- `Vector` supports three encodings: `Flat(FlatVector)`, `Constant { value, logical_type, len }`, and `Dictionary { dict, indices, validity }`, supporting arbitrarily nested dictionary indirection and seamless `flatten()`.

### 2.5 Total Ordering & Normalized Sort Keys (`encode_sort_key`)
- Floating point (`Float64`): `-0.0` is canonicalized to `+0.0` for equality, hashing, and sort keys. `NaN` is canonicalized to a single positive quiet NaN ordered **greater than `+inf`** (matching PostgreSQL/DuckDB SQL semantics).
- Multi-column binary sort keys support all 4 `(ASC|DESC) × (NULLS FIRST|NULLS LAST)` combinations:
  - Null tag byte: `0x00` for NULL when `nulls_first`, `0xFF` for NULL when `!nulls_first`; non-null uses `0x01` (or `0xFE` when `!nulls_first`? Simpler & cleaner: `nulls_first => NULL=0x00, NON_NULL=0x01`; `!nulls_first => NON_NULL=0x00, NULL=0x01` — completely independent of ASC/DESC!).
  - Fixed-width values encode into order-preserving big-endian bytes, then bitwise-inverted (`^ 0xFF`) if `DESC`.
  - Variable-length `Utf8`/`Binary` values encode using 8-byte chunked escape-free `0x00`-terminated framing (each byte `b`: if `b == 0x00` write `[0x00, 0x01]`, else `[b]`, terminated by `[0x00, 0x00]`) before applying `^ 0xFF` for `DESC`, guaranteeing exact lexicographical `memcmp` order even with embedded NUL bytes and arbitrary prefix relationships across composite keys!

## 3. Edge Cases Audited
- Empty `DataChunk` (`count == 0`), 1-row chunk, exact `VECTOR_CAPACITY` (2048), and >2048 flat vectors.
- All-NULL `Flat`, `Constant(Null)`, and `Dictionary` vectors with masked dictionary indices.
- `Decimal128` overflow boundary (`i128::MIN`, `i128::MAX`, precision 38, negative scales, 256-bit intermediate product).
- IEEE-754 `-0.0`, `+0.0`, `-inf`, `+inf`, subnormals, signaling/quiet/negative `NaN`.
- Strings of lengths `0, 1, 3, 4, 11, 12, 13, 16, 1000`, embedded `\0`, multi-byte UTF-8.
- Quota exhaustion inside `AlignedBufferMut::reserve`, `FlatVector::from_scalars`, `TupleBuffer::append_chunk`.
