# M02 Design Document: `vectra-expr`

## 1. Objectives & Architectural Guarantees
`vectra-expr` implements the late-materialization Vectorized Expression Virtual Machine operating over `DataChunk` + `SelectionVector` batches with Kleene 3-valued logic, quarantined SIMD kernels (`AVX2` / `NEON` with bit-identical safe scalar fallback), linear-time `LIKE` matching, built-in scalar functions, and vectorized `xxHash64` column hashing.

## 2. Core Components & Invariants

### 2.1 Kleene Three-Valued Logic (`kleene.rs`)
- Word-level 64-bit bitwise truth tables operate on `(value_word: u64, valid_word: u64)` representing 64 rows simultaneously:
  - `a_true = a_valid & a_val`, `a_false = a_valid & !a_val`.
  - `AND`: `out_val = a_true & b_true`, `out_valid = out_val | a_false | b_false`.
  - `OR`: `out_val = a_true | b_true`, `out_valid = out_val | (a_false & b_false)`.
  - `NOT`: `out_val = !a_val & a_valid`, `out_valid = a_valid`.
- Short-circuit `SelectionVector` narrowing for `CASE WHEN` and `COALESCE`:
  - Maintains `remaining_sel` of unassigned row positions.
  - Each `WHEN` predicate evaluates ONLY on `remaining_sel`; matching rows evaluate `THEN` on the matched sub-selection and scatter into the output buffer; non-matching rows shrink `remaining_sel`.
  - Prevents division-by-zero or invalid casts inside unreached `THEN` / `ELSE` / `COALESCE` branches (matching SQL short-circuit semantics).

### 2.2 Quarantined SIMD Kernels (`simd.rs`)
- `unsafe` is strictly isolated inside `vectra-expr::simd`, guarded by `#[cfg(all(target_arch = "x86_64", not(miri)))]` with runtime `is_x86_feature_detected!("avx2")` detection and `#[cfg(all(target_arch = "aarch64", not(miri)))]` NEON paths.
- Every SIMD kernel (`filter_*_i64`, `filter_*_i32`, `filter_*_f64`, `add_f64`, `sub_f64`, `mul_f64`, `add_i64_checked`) has an identical safe scalar counterpart; M02 verification asserts 100% bit-for-bit equality between SIMD and scalar paths across adversarial values (`i64::MIN`, `i64::MAX`, `-0.0`, `+0.0`, `NaN`, `-inf`, `+inf`).
- AVX2 filter selection uses `_mm256_cmpgt_epi64` / `_mm256_cmpeq_epi64` / `_mm256_cmp_pd` + `_mm256_movemask_pd` + `trailing_zeros()` bit extraction into `Vec<u16>` selection indices.

### 2.3 Linear-Time `LIKE` Matcher (`like.rs`)
- Compiles constant patterns into fast variants:
  1. `Exact(String)` -> O(1) `ByteView` length+prefix rejection.
  2. `Prefix(String)` (`'abc%'`) -> `starts_with`.
  3. `Suffix(String)` (`'%xyz'`) -> `ends_with`.
  4. `Contains(String)` (`'%mid%'`) -> `memchr::memmem::Finder`.
  5. `General` -> iterative two-pointer / DP NFA operating on UTF-8 `char` slices with worst-case $O(|text| \cdot |pattern|)$ time (zero catastrophic backtracking on `'%a%a%a%a%b'`).

### 2.4 `IN (...)` List Semantics & Hash-Set Fast Path
- Preserves strict SQL 3-valued NULL semantics:
  - Empty list: `x IN () => FALSE`, `x NOT IN () => TRUE` (even when `x` is NULL).
  - Constant list with `len >= 8`: builds a `HashSet<ScalarValue>` + `has_null: bool` once per chunk for $O(1)$ lookup per row.
  - Non-null `x`: if in set -> `TRUE` (or `FALSE` if negated); else if `has_null` -> `NULL`; else -> `FALSE` (or `TRUE` if negated).

### 2.5 Vectorized Hashing (`hash.rs`)
- `hash_chunk` computes deterministic 64-bit hashes over arbitrary columns and `SelectionVector`s.
- `ConstantVector` hashes its single scalar once in $O(1)$ and broadcasts across rows.
- Normalizes `-0.0` to `+0.0` and canonicalizes `NaN` so equal values ALWAYS hash identically regardless of encoding (`Flat`, `Constant`, `Dictionary`).
