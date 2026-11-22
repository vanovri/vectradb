use std::sync::Arc;

/// Optional `u16` row-index indirection buffer enabling DuckDB/MonetDB-style
/// late materialization and zero-copy predicate filtering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionVector {
    /// Zero-overhead identity mapping `i -> i` for `0..len`.
    Identity(usize),
    /// Explicit row indices into an underlying `DataChunk` or dictionary `Vector`.
    Indices(Arc<[u16]>),
}

impl SelectionVector {
    #[inline]
    pub const fn identity(len: usize) -> Self {
        Self::Identity(len)
    }

    #[inline]
    pub fn from_vec(indices: Vec<u16>) -> Self {
        Self::Indices(Arc::from(indices))
    }

    #[inline]
    pub fn from_slice(indices: &[u16]) -> Self {
        Self::Indices(Arc::from(indices))
    }

    #[inline]
    pub fn len(&self) -> usize {
        match self {
            Self::Identity(n) => *n,
            Self::Indices(idx) => idx.len(),
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn is_identity(&self) -> bool {
        matches!(self, Self::Identity(_))
    }

    #[inline]
    pub fn get(&self, pos: usize) -> usize {
        match self {
            Self::Identity(n) => {
                debug_assert!(pos < *n, "SelectionVector::Identity({n}) index {pos} OOB");
                pos
            }
            Self::Indices(idx) => idx[pos] as usize,
        }
    }

    #[inline]
    pub fn as_indices(&self) -> Option<&[u16]> {
        match self {
            Self::Identity(_) => None,
            Self::Indices(idx) => Some(idx),
        }
    }

    /// Composes two selection vectors: `result[i] = self[other[i]]`.
    /// Used when applying a second filter `other` on top of an already-filtered chunk `self`.
    pub fn compose(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::Identity(n), Self::Identity(m)) => {
                assert!(
                    *m <= *n,
                    "compose Identity({m}) exceeds parent Identity({n})"
                );
                Self::Identity(*m)
            }
            (Self::Identity(n), Self::Indices(b)) => {
                debug_assert!(b.iter().all(|&i| (i as usize) < *n));
                Self::Indices(Arc::clone(b))
            }
            (Self::Indices(a), Self::Identity(m)) => {
                assert!(*m <= a.len());
                if *m == a.len() {
                    Self::Indices(Arc::clone(a))
                } else {
                    Self::from_slice(&a[..*m])
                }
            }
            (Self::Indices(a), Self::Indices(b)) => {
                let mut out = Vec::with_capacity(b.len());
                for &idx in b.iter() {
                    out.push(a[idx as usize]);
                }
                Self::from_vec(out)
            }
        }
    }

    /// Returns a sliced selection vector over logical positions `[offset..offset + len)`.
    pub fn slice(&self, offset: usize, len: usize) -> Self {
        assert!(
            offset <= self.len() && len <= self.len() - offset,
            "SelectionVector::slice({offset}, {len}) OOB for len {}",
            self.len()
        );
        match self {
            Self::Identity(_) => {
                if offset == 0 {
                    Self::Identity(len)
                } else {
                    let v: Vec<u16> = (offset..offset + len)
                        .map(|i| u16::try_from(i).expect("selection index fits in u16"))
                        .collect();
                    Self::from_vec(v)
                }
            }
            Self::Indices(idx) => Self::from_slice(&idx[offset..offset + len]),
        }
    }

    #[inline]
    pub fn iter(&self) -> SelectionIter<'_> {
        match self {
            Self::Identity(n) => SelectionIter::Identity { cur: 0, len: *n },
            Self::Indices(idx) => SelectionIter::Indices { slice: idx, pos: 0 },
        }
    }
}

pub enum SelectionIter<'a> {
    Identity { cur: usize, len: usize },
    Indices { slice: &'a [u16], pos: usize },
}

impl Iterator for SelectionIter<'_> {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<usize> {
        match self {
            Self::Identity { cur, len } => {
                if *cur < *len {
                    let v = *cur;
                    *cur += 1;
                    Some(v)
                } else {
                    None
                }
            }
            Self::Indices { slice, pos } => {
                if *pos < slice.len() {
                    let v = slice[*pos] as usize;
                    *pos += 1;
                    Some(v)
                } else {
                    None
                }
            }
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let rem = match self {
            Self::Identity { cur, len } => *len - *cur,
            Self::Indices { slice, pos } => slice.len() - *pos,
        };
        (rem, Some(rem))
    }
}

impl ExactSizeIterator for SelectionIter<'_> {}
