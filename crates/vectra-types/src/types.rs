use crate::date::{format_date32, format_timestamp_micros};
use crate::decimal::{decimal_rescale, format_decimal, MAX_DECIMAL_PRECISION};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

/// Canonical logical data types supported end-to-end across VectraDB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LogicalType {
    Null,
    Boolean,
    Int8,
    Int16,
    Int32,
    Int64,
    Float64,
    Decimal128 { precision: u8, scale: i8 },
    Date32,
    TimestampMicros,
    Utf8,
    Binary,
}

impl LogicalType {
    /// Fixed slot byte width inside a `FlatVector` data buffer.
    /// Variable-length types (`Utf8`, `Binary`) store 16-byte `ByteView` slots.
    #[inline]
    pub const fn fixed_byte_width(&self) -> usize {
        match self {
            Self::Null => 0,
            Self::Boolean | Self::Int8 => 1,
            Self::Int16 => 2,
            Self::Int32 | Self::Date32 => 4,
            Self::Int64 | Self::Float64 | Self::TimestampMicros => 8,
            Self::Decimal128 { .. } | Self::Utf8 | Self::Binary => 16,
        }
    }

    #[inline]
    pub const fn is_varlen(&self) -> bool {
        matches!(self, Self::Utf8 | Self::Binary)
    }

    #[inline]
    pub const fn is_numeric(&self) -> bool {
        matches!(
            self,
            Self::Int8
                | Self::Int16
                | Self::Int32
                | Self::Int64
                | Self::Float64
                | Self::Decimal128 { .. }
        )
    }

    #[inline]
    pub const fn is_integer(&self) -> bool {
        matches!(self, Self::Int8 | Self::Int16 | Self::Int32 | Self::Int64)
    }

    /// Standard PostgreSQL OID for wire protocol (`M09`) compatibility.
    #[inline]
    pub const fn pg_oid(&self) -> i32 {
        match self {
            Self::Null => 705,               // UNKNOWN
            Self::Boolean => 16,             // BOOL
            Self::Int8 | Self::Int16 => 21,  // INT2
            Self::Int32 => 23,               // INT4
            Self::Int64 => 20,               // INT8
            Self::Float64 => 701,            // FLOAT8
            Self::Decimal128 { .. } => 1700, // NUMERIC
            Self::Date32 => 1082,            // DATE
            Self::TimestampMicros => 1114,   // TIMESTAMP
            Self::Utf8 => 25,                // TEXT
            Self::Binary => 17,              // BYTEA
        }
    }
}

impl fmt::Display for LogicalType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => write!(f, "NULL"),
            Self::Boolean => write!(f, "BOOLEAN"),
            Self::Int8 => write!(f, "TINYINT"),
            Self::Int16 => write!(f, "SMALLINT"),
            Self::Int32 => write!(f, "INTEGER"),
            Self::Int64 => write!(f, "BIGINT"),
            Self::Float64 => write!(f, "DOUBLE"),
            Self::Decimal128 { precision, scale } => write!(f, "DECIMAL({precision},{scale})"),
            Self::Date32 => write!(f, "DATE"),
            Self::TimestampMicros => write!(f, "TIMESTAMP"),
            Self::Utf8 => write!(f, "VARCHAR"),
            Self::Binary => write!(f, "VARBINARY"),
        }
    }
}

/// Canonicalizes IEEE-754 floats so `-0.0` maps to `+0.0` and all `NaN`s map to one quiet `NaN`.
#[inline]
pub fn canonicalize_f64(v: f64) -> f64 {
    if v.is_nan() {
        f64::from_bits(0x7ff8_0000_0000_0000)
    } else if v == 0.0 {
        0.0
    } else {
        v
    }
}

/// SQL/PostgreSQL canonical total order on `f64`:
/// `-inf < negative < (-0.0 == +0.0) < positive < +inf < NaN` (all `NaN`s equal).
#[inline]
pub fn f64_total_cmp(a: f64, b: f64) -> Ordering {
    let a = canonicalize_f64(a);
    let b = canonicalize_f64(b);
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a.total_cmp(&b),
    }
}

/// Single scalar runtime value corresponding to a `LogicalType`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScalarValue {
    Null,
    Boolean(bool),
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Float64(f64),
    Decimal128 {
        value: i128,
        precision: u8,
        scale: i8,
    },
    Date32(i32),
    TimestampMicros(i64),
    Utf8(String),
    Binary(Vec<u8>),
}

impl ScalarValue {
    #[inline]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn logical_type(&self) -> LogicalType {
        match self {
            Self::Null => LogicalType::Null,
            Self::Boolean(_) => LogicalType::Boolean,
            Self::Int8(_) => LogicalType::Int8,
            Self::Int16(_) => LogicalType::Int16,
            Self::Int32(_) => LogicalType::Int32,
            Self::Int64(_) => LogicalType::Int64,
            Self::Float64(_) => LogicalType::Float64,
            Self::Decimal128 {
                precision, scale, ..
            } => LogicalType::Decimal128 {
                precision: *precision,
                scale: *scale,
            },
            Self::Date32(_) => LogicalType::Date32,
            Self::TimestampMicros(_) => LogicalType::TimestampMicros,
            Self::Utf8(_) => LogicalType::Utf8,
            Self::Binary(_) => LogicalType::Binary,
        }
    }

    /// Canonical total ordering with configurable `nulls_first`.
    /// Cross-type comparisons fall back to variant discriminant so total ordering never panics.
    pub fn total_cmp_with_nulls(&self, other: &Self, nulls_first: bool) -> Ordering {
        match (self, other) {
            (Self::Null, Self::Null) => Ordering::Equal,
            (Self::Null, _) => {
                if nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (_, Self::Null) => {
                if nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (Self::Boolean(a), Self::Boolean(b)) => a.cmp(b),
            (Self::Int8(a), Self::Int8(b)) => a.cmp(b),
            (Self::Int16(a), Self::Int16(b)) => a.cmp(b),
            (Self::Int32(a), Self::Int32(b)) => a.cmp(b),
            (Self::Int64(a), Self::Int64(b)) => a.cmp(b),
            (Self::Float64(a), Self::Float64(b)) => f64_total_cmp(*a, *b),
            (
                Self::Decimal128 {
                    value: va,
                    scale: sa,
                    ..
                },
                Self::Decimal128 {
                    value: vb,
                    scale: sb,
                    ..
                },
            ) => {
                if sa == sb {
                    va.cmp(vb)
                } else {
                    let target = (*sa).max(*sb);
                    match (
                        decimal_rescale(*va, *sa, target, MAX_DECIMAL_PRECISION),
                        decimal_rescale(*vb, *sb, target, MAX_DECIMAL_PRECISION),
                    ) {
                        (Ok(ra), Ok(rb)) => ra.cmp(&rb),
                        _ => {
                            let fa = (*va as f64) / 10f64.powi(*sa as i32);
                            let fb = (*vb as f64) / 10f64.powi(*sb as i32);
                            f64_total_cmp(fa, fb)
                        }
                    }
                }
            }
            (Self::Date32(a), Self::Date32(b)) => a.cmp(b),
            (Self::TimestampMicros(a), Self::TimestampMicros(b)) => a.cmp(b),
            (Self::Utf8(a), Self::Utf8(b)) => a.as_bytes().cmp(b.as_bytes()),
            (Self::Binary(a), Self::Binary(b)) => a.cmp(b),
            (a, b) => a.type_rank().cmp(&b.type_rank()),
        }
    }

    /// Canonical total ordering defaulting to `NULLS LAST` (PostgreSQL `ASC` default).
    #[inline]
    pub fn total_cmp(&self, other: &Self) -> Ordering {
        self.total_cmp_with_nulls(other, false)
    }

    #[inline]
    fn type_rank(&self) -> u8 {
        match self {
            Self::Null => 0,
            Self::Boolean(_) => 1,
            Self::Int8(_) => 2,
            Self::Int16(_) => 3,
            Self::Int32(_) => 4,
            Self::Int64(_) => 5,
            Self::Float64(_) => 6,
            Self::Decimal128 { .. } => 7,
            Self::Date32(_) => 8,
            Self::TimestampMicros(_) => 9,
            Self::Utf8(_) => 10,
            Self::Binary(_) => 11,
        }
    }

    /// Formats the scalar for SQL / CLI / wire text display.
    pub fn to_display_string(&self) -> String {
        match self {
            Self::Null => "NULL".to_string(),
            Self::Boolean(b) => if *b { "true" } else { "false" }.to_string(),
            Self::Int8(v) => itoa::Buffer::new().format(*v).to_string(),
            Self::Int16(v) => itoa::Buffer::new().format(*v).to_string(),
            Self::Int32(v) => itoa::Buffer::new().format(*v).to_string(),
            Self::Int64(v) => itoa::Buffer::new().format(*v).to_string(),
            Self::Float64(v) => {
                let c = canonicalize_f64(*v);
                if c.is_nan() {
                    "NaN".to_string()
                } else if c.is_infinite() {
                    if c.is_sign_positive() {
                        "Infinity".to_string()
                    } else {
                        "-Infinity".to_string()
                    }
                } else {
                    ryu::Buffer::new().format(c).to_string()
                }
            }
            Self::Decimal128 { value, scale, .. } => format_decimal(*value, *scale),
            Self::Date32(d) => format_date32(*d),
            Self::TimestampMicros(us) => format_timestamp_micros(*us),
            Self::Utf8(s) => s.clone(),
            Self::Binary(b) => {
                let mut out = String::with_capacity(2 + b.len() * 2);
                out.push_str("\\x");
                for byte in b {
                    use std::fmt::Write;
                    let _ = write!(&mut out, "{byte:02x}");
                }
                out
            }
        }
    }

    /// Deterministic 64-bit hash consistent with `PartialEq` (normalizing `-0.0` and `NaN`).
    pub fn canonical_hash(&self) -> u64 {
        // wyhash-style avalanche mixer
        const SEED: u64 = 0x517c_c1b7_2722_0a95;
        let mix = |x: u64| -> u64 {
            let mut z = x.wrapping_add(SEED);
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        match self {
            Self::Null => 0x0000_0000_dead_beef,
            Self::Boolean(b) => mix(u64::from(*b)),
            Self::Int8(v) => mix(*v as u64),
            Self::Int16(v) => mix(*v as u64),
            Self::Int32(v) => mix(*v as u64),
            Self::Int64(v) => mix(*v as u64),
            Self::Float64(v) => mix(canonicalize_f64(*v).to_bits()),
            Self::Decimal128 { value, .. } => {
                let lo = *value as u64;
                let hi = (*value >> 64) as u64;
                mix(lo ^ mix(hi))
            }
            Self::Date32(d) => mix(*d as u64),
            Self::TimestampMicros(us) => mix(*us as u64),
            Self::Utf8(s) => hash_bytes_xx64(s.as_bytes()),
            Self::Binary(b) => hash_bytes_xx64(b),
        }
    }
}

/// Fast, deterministic XXH64-inspired 64-bit byte slice hash.
#[inline]
pub fn hash_bytes_xx64(bytes: &[u8]) -> u64 {
    const P1: u64 = 0x9e37_79b1_85eb_ca87;
    const P2: u64 = 0xc2b2_ae3d_27d4_eb4f;
    const P3: u64 = 0x1656_67b1_9e37_79f9;
    const P4: u64 = 0x85eb_ca77_c2b2_ae63;
    const P5: u64 = 0x27d4_eb2f_1656_67c5;

    let mut h = P5.wrapping_add(bytes.len() as u64);
    let (chunks, rem) = bytes.as_chunks::<8>();
    for chunk in chunks {
        let mut k = u64::from_le_bytes(*chunk);
        k = k.wrapping_mul(P2).rotate_left(31).wrapping_mul(P1);
        h ^= k;
        h = h.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
    }
    let mut i = 0;
    while i + 4 <= rem.len() {
        let k = u32::from_le_bytes(rem[i..i + 4].try_into().expect("4 bytes")) as u64;
        h ^= k.wrapping_mul(P1);
        h = h.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        i += 4;
    }
    while i < rem.len() {
        h ^= (rem[i] as u64).wrapping_mul(P5);
        h = h.rotate_left(11).wrapping_mul(P1);
        i += 1;
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^= h >> 32;
    h
}

impl PartialEq for ScalarValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Boolean(a), Self::Boolean(b)) => a == b,
            (Self::Int8(a), Self::Int8(b)) => a == b,
            (Self::Int16(a), Self::Int16(b)) => a == b,
            (Self::Int32(a), Self::Int32(b)) => a == b,
            (Self::Int64(a), Self::Int64(b)) => a == b,
            (Self::Float64(a), Self::Float64(b)) => f64_total_cmp(*a, *b) == Ordering::Equal,
            (Self::Decimal128 { .. }, Self::Decimal128 { .. }) => {
                self.total_cmp(other) == Ordering::Equal
            }
            (Self::Date32(a), Self::Date32(b)) => a == b,
            (Self::TimestampMicros(a), Self::TimestampMicros(b)) => a == b,
            (Self::Utf8(a), Self::Utf8(b)) => a == b,
            (Self::Binary(a), Self::Binary(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for ScalarValue {}

impl Hash for ScalarValue {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.type_rank().hash(state);
        self.canonical_hash().hash(state);
    }
}

impl fmt::Display for ScalarValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_display_string())
    }
}

/// Column field descriptor inside a relation `Schema`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub logical_type: LogicalType,
    pub nullable: bool,
}

impl Field {
    pub fn new(name: impl Into<String>, logical_type: LogicalType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            logical_type,
            nullable,
        }
    }
}

/// Ordered list of named, typed columns.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Schema {
    pub fields: Vec<Field>,
}

impl Schema {
    pub fn new(fields: Vec<Field>) -> Self {
        Self { fields }
    }

    pub fn empty() -> Self {
        Self { fields: Vec::new() }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn field(&self, idx: usize) -> &Field {
        &self.fields[idx]
    }

    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.fields
            .iter()
            .position(|f| f.name.eq_ignore_ascii_case(name))
    }

    pub fn logical_types(&self) -> Vec<LogicalType> {
        self.fields.iter().map(|f| f.logical_type).collect()
    }
}
