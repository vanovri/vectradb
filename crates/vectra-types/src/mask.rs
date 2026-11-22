use std::sync::Arc;

/// Standard vectorized execution batch size (2,048 tuples = 32 x 64-bit mask words).
pub const VECTOR_CAPACITY: usize = 2048;

/// Number of `u64` words required to represent `len` validity bits.
#[inline]
pub const fn mask_word_count(len: usize) -> usize {
    len.div_ceil(64)
}

/// Masks out unused trailing bits in the final word so `count_ones` and word-level
/// comparisons remain exact.
#[inline]
pub const fn trailing_word_mask(len: usize) -> u64 {
    let rem = len % 64;
    if rem == 0 {
        u64::MAX
    } else {
        (1u64 << rem) - 1
    }
}

/// Copy-on-write optional validity bitset (`1` = valid / non-null, `0` = NULL).
/// `bits == None` is an unallocated O(1) fast path meaning all `len` entries are valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidityMask {
    bits: Option<Arc<[u64]>>,
    len: usize,
}

impl ValidityMask {
    /// Zero-allocation all-valid mask of length `len`.
    #[inline]
    pub const fn all_valid(len: usize) -> Self {
        Self { bits: None, len }
    }

    /// All-NULL mask of length `len`.
    pub fn all_null(len: usize) -> Self {
        if len == 0 {
            return Self::all_valid(0);
        }
        let words = vec![0u64; mask_word_count(len)];
        Self {
            bits: Some(Arc::from(words)),
            len,
        }
    }

    /// Constructs a validity mask from a slice of booleans (`true` = valid).
    /// Collapses automatically to `None` if every element is `true`.
    pub fn from_bools(valid: &[bool]) -> Self {
        let len = valid.len();
        if valid.iter().all(|&b| b) {
            return Self::all_valid(len);
        }
        let mut words = vec![0u64; mask_word_count(len)];
        for (i, &is_v) in valid.iter().enumerate() {
            if is_v {
                words[i / 64] |= 1u64 << (i % 64);
            }
        }
        Self {
            bits: Some(Arc::from(words)),
            len,
        }
    }

    /// Constructs from raw packed `u64` words, sanitizing trailing bits and collapsing all-valid.
    pub fn from_words(mut words: Vec<u64>, len: usize) -> Self {
        let expected = mask_word_count(len);
        assert_eq!(words.len(), expected, "word count mismatch for len {len}");
        if len == 0 {
            return Self::all_valid(0);
        }
        if let Some(last) = words.last_mut() {
            *last &= trailing_word_mask(len);
        }
        let all_set = words[..expected - 1].iter().all(|&w| w == u64::MAX)
            && words[expected - 1] == trailing_word_mask(len);
        if all_set {
            Self::all_valid(len)
        } else {
            Self {
                bits: Some(Arc::from(words)),
                len,
            }
        }
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
    pub fn is_all_valid(&self) -> bool {
        self.bits.is_none()
    }

    #[inline]
    pub fn raw_words(&self) -> Option<&[u64]> {
        self.bits.as_deref()
    }

    #[inline]
    pub fn is_valid(&self, idx: usize) -> bool {
        debug_assert!(
            idx < self.len,
            "ValidityMask index {idx} >= len {}",
            self.len
        );
        match &self.bits {
            None => true,
            Some(words) => (words[idx >> 6] & (1u64 << (idx & 63))) != 0,
        }
    }

    #[inline]
    pub fn is_null(&self, idx: usize) -> bool {
        !self.is_valid(idx)
    }

    /// Counts non-null bits using hardware `POPCNT` (`u64::count_ones`).
    #[inline]
    pub fn count_valid(&self) -> usize {
        match &self.bits {
            None => self.len,
            Some(words) => words.iter().map(|w| w.count_ones() as usize).sum(),
        }
    }

    #[inline]
    pub fn count_nulls(&self) -> usize {
        self.len - self.count_valid()
    }

    /// Bitwise AND of two masks of equal length (both inputs must be valid).
    pub fn bitand(&self, other: &Self) -> Self {
        assert_eq!(self.len, other.len, "ValidityMask::bitand length mismatch");
        match (&self.bits, &other.bits) {
            (None, None) => Self::all_valid(self.len),
            (None, Some(_)) => other.clone(),
            (Some(_), None) => self.clone(),
            (Some(a), Some(b)) => {
                let words: Vec<u64> = a.iter().zip(b.iter()).map(|(&x, &y)| x & y).collect();
                Self::from_words(words, self.len)
            }
        }
    }

    /// Bitwise OR of two masks of equal length.
    pub fn bitor(&self, other: &Self) -> Self {
        assert_eq!(self.len, other.len, "ValidityMask::bitor length mismatch");
        match (&self.bits, &other.bits) {
            (None, _) | (_, None) => Self::all_valid(self.len),
            (Some(a), Some(b)) => {
                let words: Vec<u64> = a.iter().zip(b.iter()).map(|(&x, &y)| x | y).collect();
                Self::from_words(words, self.len)
            }
        }
    }

    /// Bitwise NOT (flips valid <-> null).
    pub fn bitnot(&self) -> Self {
        if self.len == 0 {
            return Self::all_valid(0);
        }
        match &self.bits {
            None => Self::all_null(self.len),
            Some(a) => {
                let words: Vec<u64> = a.iter().map(|&x| !x).collect();
                Self::from_words(words, self.len)
            }
        }
    }

    /// Extracts a sub-range `[offset..offset + len)` shifting bit positions accurately.
    pub fn slice(&self, offset: usize, len: usize) -> Self {
        assert!(
            offset <= self.len && len <= self.len - offset,
            "ValidityMask::slice({offset}, {len}) out of bounds for len {}",
            self.len
        );
        if len == 0 {
            return Self::all_valid(0);
        }
        let Some(words) = &self.bits else {
            return Self::all_valid(len);
        };
        let out_words_n = mask_word_count(len);
        let mut out = vec![0u64; out_words_n];
        let word_off = offset >> 6;
        let bit_off = offset & 63;
        if bit_off == 0 {
            out.copy_from_slice(&words[word_off..word_off + out_words_n]);
        } else {
            for (i, dst) in out.iter_mut().enumerate() {
                let lo = words[word_off + i] >> bit_off;
                let hi = if word_off + i + 1 < words.len() {
                    words[word_off + i + 1] << (64 - bit_off)
                } else {
                    0
                };
                *dst = lo | hi;
            }
        }
        Self::from_words(out, len)
    }

    /// Iterates over every valid row index using hardware `TZCNT` (`trailing_zeros`).
    #[inline]
    pub fn iter_valid_indices(&self) -> ValidIndicesIter<'_> {
        match &self.bits {
            None => ValidIndicesIter::AllValid {
                cur: 0,
                len: self.len,
            },
            Some(words) => ValidIndicesIter::Bitmap {
                words,
                word_idx: 0,
                cur_word: words.first().copied().unwrap_or(0),
            },
        }
    }

    /// Packs validity bits into a contiguous byte vector (for serialization/storage codecs).
    pub fn to_bytes(&self) -> Vec<u8> {
        let num_bytes = self.len.div_ceil(8);
        let Some(words) = &self.bits else {
            let mut out = vec![0xFFu8; num_bytes];
            let rem = self.len % 8;
            if rem != 0 {
                if let Some(last) = out.last_mut() {
                    *last = (1u8 << rem) - 1;
                }
            }
            return out;
        };
        let mut out = Vec::with_capacity(num_bytes);
        for &w in words.iter() {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.truncate(num_bytes);
        out
    }

    /// Reconstructs a `ValidityMask` from packed little-endian bytes.
    pub fn from_bytes(bytes: &[u8], len: usize) -> Self {
        let num_words = mask_word_count(len);
        let mut words = vec![0u64; num_words];
        for (i, chunk) in bytes.chunks(8).enumerate().take(num_words) {
            let mut buf = [0u8; 8];
            buf[..chunk.len()].copy_from_slice(chunk);
            words[i] = u64::from_le_bytes(buf);
        }
        Self::from_words(words, len)
    }
}

/// Fast word-level iterator over set bits in a `ValidityMask`.
pub enum ValidIndicesIter<'a> {
    AllValid {
        cur: usize,
        len: usize,
    },
    Bitmap {
        words: &'a [u64],
        word_idx: usize,
        cur_word: u64,
    },
}

impl Iterator for ValidIndicesIter<'_> {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<usize> {
        match self {
            Self::AllValid { cur, len } => {
                if *cur < *len {
                    let idx = *cur;
                    *cur += 1;
                    Some(idx)
                } else {
                    None
                }
            }
            Self::Bitmap {
                words,
                word_idx,
                cur_word,
            } => {
                while *cur_word == 0 {
                    *word_idx += 1;
                    if *word_idx >= words.len() {
                        return None;
                    }
                    *cur_word = words[*word_idx];
                }
                let tz = cur_word.trailing_zeros() as usize;
                *cur_word &= *cur_word - 1;
                Some((*word_idx << 6) + tz)
            }
        }
    }
}

/// Mutable builder for constructing `ValidityMask`s incrementally without reallocating
/// until the first NULL is encountered.
#[derive(Debug, Clone)]
pub struct ValidityMaskBuilder {
    words: Option<Vec<u64>>,
    len: usize,
}

impl ValidityMaskBuilder {
    pub fn with_capacity(cap: usize) -> Self {
        let _ = cap;
        Self {
            words: None,
            len: 0,
        }
    }

    pub fn new_all_valid(len: usize) -> Self {
        Self { words: None, len }
    }

    #[inline]
    pub fn push(&mut self, valid: bool) {
        let idx = self.len;
        if valid {
            if let Some(words) = &mut self.words {
                let w = idx >> 6;
                if w >= words.len() {
                    words.resize(w + 1, 0);
                }
                words[w] |= 1u64 << (idx & 63);
            }
        } else {
            let words = self.words.get_or_insert_with(|| {
                let mut w = vec![u64::MAX; mask_word_count(idx.max(1))];
                if idx > 0 {
                    let rem = idx % 64;
                    if rem != 0 {
                        if let Some(last) = w.last_mut() {
                            *last = (1u64 << rem) - 1;
                        }
                    }
                } else {
                    w[0] = 0;
                }
                w
            });
            let w = idx >> 6;
            if w >= words.len() {
                words.resize(w + 1, 0);
            }
            words[w] &= !(1u64 << (idx & 63));
        }
        self.len += 1;
    }

    #[inline]
    pub fn set(&mut self, idx: usize, valid: bool) {
        assert!(idx < self.len);
        if valid && self.words.is_none() {
            return;
        }
        let words = self.words.get_or_insert_with(|| {
            let mut w = vec![u64::MAX; mask_word_count(self.len)];
            if let Some(last) = w.last_mut() {
                *last &= trailing_word_mask(self.len);
            }
            w
        });
        if valid {
            words[idx >> 6] |= 1u64 << (idx & 63);
        } else {
            words[idx >> 6] &= !(1u64 << (idx & 63));
        }
    }

    pub fn finish(self) -> ValidityMask {
        match self.words {
            None => ValidityMask::all_valid(self.len),
            Some(mut w) => {
                w.resize(mask_word_count(self.len), 0);
                ValidityMask::from_words(w, self.len)
            }
        }
    }
}
