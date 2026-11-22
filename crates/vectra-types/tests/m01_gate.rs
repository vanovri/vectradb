use std::cmp::Ordering;
use std::sync::Arc;
use vectra_types::*;

fn gen_scalar(rng: &mut SplitMix64, ty: LogicalType) -> ScalarValue {
    if rng.gen_bool(0.18) {
        return ScalarValue::Null;
    }
    match ty {
        LogicalType::Null => ScalarValue::Null,
        LogicalType::Boolean => ScalarValue::Boolean(rng.gen_bool(0.5)),
        LogicalType::Int8 => ScalarValue::Int8(match rng.next_usize(6) {
            0 => i8::MIN,
            1 => i8::MAX,
            2 => 0,
            _ => rng.next_u32() as i8,
        }),
        LogicalType::Int16 => ScalarValue::Int16(match rng.next_usize(6) {
            0 => i16::MIN,
            1 => i16::MAX,
            2 => 0,
            _ => rng.next_u32() as i16,
        }),
        LogicalType::Int32 => ScalarValue::Int32(match rng.next_usize(6) {
            0 => i32::MIN,
            1 => i32::MAX,
            2 => 0,
            _ => rng.next_u32() as i32,
        }),
        LogicalType::Int64 => ScalarValue::Int64(match rng.next_usize(7) {
            0 => i64::MIN,
            1 => i64::MAX,
            2 => 0,
            3 => -1,
            _ => rng.next_u64() as i64,
        }),
        LogicalType::Float64 => ScalarValue::Float64(match rng.next_usize(10) {
            0 => -0.0,
            1 => 0.0,
            2 => f64::NAN,
            3 => -f64::NAN,
            4 => f64::INFINITY,
            5 => f64::NEG_INFINITY,
            6 => f64::MIN_POSITIVE / 16.0,
            7 => -f64::MIN_POSITIVE / 16.0,
            _ => (rng.next_f64_unit() - 0.5) * 1e12,
        }),
        LogicalType::Decimal128 { precision, scale } => {
            let max_mag = pow10_i128(precision.min(18)).unwrap() - 1;
            let raw = (rng.next_u64() as i128) % (max_mag + 1);
            let value = if rng.gen_bool(0.5) { -raw } else { raw };
            ScalarValue::Decimal128 {
                value,
                precision,
                scale,
            }
        }
        LogicalType::Date32 => ScalarValue::Date32(rng.gen_range_i64(-50_000, 50_000) as i32),
        LogicalType::TimestampMicros => {
            ScalarValue::TimestampMicros(rng.gen_range_i64(-1_000_000_000_000, 2_000_000_000_000))
        }
        LogicalType::Utf8 => {
            let len = match rng.next_usize(8) {
                0 => 0,
                1 => 1,
                2 => 4,
                3 => 12,
                4 => 13,
                5 => 24,
                _ => rng.next_usize(48),
            };
            let alphabet = ["a", "b", "z", "\0", "é", "🚀", "中", "A", "0"];
            let mut s = String::new();
            while s.len() < len {
                s.push_str(rng.choose(&alphabet));
            }
            ScalarValue::Utf8(s)
        }
        LogicalType::Binary => {
            let len = match rng.next_usize(7) {
                0 => 0,
                1 => 1,
                2 => 4,
                3 => 12,
                4 => 13,
                _ => rng.next_usize(36),
            };
            let mut b = Vec::with_capacity(len);
            for _ in 0..len {
                let v = match rng.next_usize(5) {
                    0 => 0x00,
                    1 => 0x01,
                    2 => 0xFF,
                    _ => (rng.next_u32() & 0xFF) as u8,
                };
                b.push(v);
            }
            ScalarValue::Binary(b)
        }
    }
}

fn gen_mixed_vector(
    rng: &mut SplitMix64,
    ty: LogicalType,
    len: usize,
    pool: &Arc<dyn MemoryPool>,
    depth: usize,
) -> Vector {
    let mode = if depth >= 2 || len == 0 {
        0
    } else {
        rng.next_usize(4)
    };
    match mode {
        // Constant vector
        1 => {
            let val = gen_scalar(rng, ty);
            Vector::constant(val, ty, len)
        }
        // Dictionary vector (possibly nested!)
        2 | 3 => {
            let dict_len = rng.next_usize(12) + 1;
            let dict = Arc::new(gen_mixed_vector(rng, ty, dict_len, pool, depth + 1));
            let mut indices = Vec::with_capacity(len);
            let mut valid = Vec::with_capacity(len);
            for _ in 0..len {
                indices.push(rng.next_usize(dict_len) as u16);
                valid.push(!rng.gen_bool(0.15));
            }
            Vector::dictionary(
                dict,
                SelectionVector::from_vec(indices),
                ValidityMask::from_bools(&valid),
            )
        }
        // Flat vector
        _ => {
            let vals: Vec<ScalarValue> = (0..len).map(|_| gen_scalar(rng, ty)).collect();
            Vector::from_scalars(&vals, ty, pool).expect("flat vector build")
        }
    }
}

fn compare_with_order(a: &ScalarValue, b: &ScalarValue, order: SortOrder) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => {
            if order.nulls_first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (false, true) => {
            if order.nulls_first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (false, false) => {
            let base = a.total_cmp(b);
            if order.ascending {
                base
            } else {
                base.reverse()
            }
        }
    }
}

#[test]
fn test_memory_pool_accounting_and_bounds() {
    let pool = BoundedMemoryPool::new(1024);
    let dyn_pool = pool.as_dyn();

    let mut r1 = dyn_pool.reserve(512).unwrap();
    assert_eq!(pool.current_bytes(), 512);
    assert_eq!(pool.peak_bytes(), 512);

    r1.try_grow(256).unwrap();
    assert_eq!(pool.current_bytes(), 768);
    assert_eq!(pool.peak_bytes(), 768);

    // Exceeding limit fails cleanly without changing current_bytes.
    let err = r1.try_grow(300).unwrap_err();
    assert!(matches!(err, VectraError::OutOfMemory { .. }));
    assert_eq!(pool.current_bytes(), 768);

    // Split + absorb preserve exact conservation.
    let r2 = r1.split(268).unwrap();
    assert_eq!(r1.bytes(), 500);
    assert_eq!(r2.bytes(), 268);
    assert_eq!(pool.current_bytes(), 768);

    r1.shrink(100);
    assert_eq!(pool.current_bytes(), 668);
    assert_eq!(pool.peak_bytes(), 768);

    r1.absorb(r2);
    assert_eq!(r1.bytes(), 668);
    drop(r1);
    pool.assert_no_leaks();
}

#[test]
fn test_aligned_buffer_alignment_and_slicing() {
    let pool = BoundedMemoryPool::unbounded();
    let dyn_pool = pool.as_dyn();

    let empty = AlignedBuffer::empty(&dyn_pool);
    assert!(empty.is_empty());
    assert_eq!((empty.as_ptr() as usize) % CACHE_LINE_ALIGN, 0);

    let data: Vec<i64> = (0..256).collect();
    let buf = AlignedBuffer::from_typed_slice(&data, &dyn_pool).unwrap();
    assert_eq!((buf.as_ptr() as usize) % CACHE_LINE_ALIGN, 0);
    assert_eq!(buf.as_typed_slice::<i64>(), &data[..]);

    let sliced = buf.slice(16, 64);
    assert_eq!(sliced.as_typed_slice::<i64>(), &data[2..10]);

    drop(sliced);
    drop(buf);
    drop(empty);
    pool.assert_no_leaks();
}

#[test]
fn test_byte_view_german_strings_exhaustive() {
    let pool = BoundedMemoryPool::unbounded();
    let dyn_pool = pool.as_dyn();

    let samples: Vec<&[u8]> = vec![
        b"",
        b"\0",
        b"\0\0",
        b"a",
        b"a\0",
        b"a\0b",
        b"ab",
        b"abcd",
        b"abcdefghijkl",   // exact 12 inline
        b"abcdefghijklm",  // exact 13 outline
        b"abcdefghijkl\0", // 13 outline with trailing NUL
        b"abcd_different_1234",
        b"abcd_different_1235",
        b"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
        "🚀🔥🎉✨🌟".as_bytes(),
    ];

    let mut builder = StringHeapBuilder::with_block_size(&dyn_pool, 64);
    let mut views = Vec::new();
    for s in &samples {
        let v = builder.append(s).unwrap();
        assert_eq!(v.is_inline(), s.len() <= BYTE_VIEW_INLINE_MAX);
        views.push(v);
    }
    let heap = builder.finish();

    for (i, &sa) in samples.iter().enumerate() {
        assert_eq!(views[i].as_bytes(&heap), sa);
        for (j, &sb) in samples.iter().enumerate() {
            assert_eq!(
                ByteView::eq_fast(&views[i], &heap, &views[j], &heap),
                sa == sb,
                "eq_fast mismatch on {sa:?} vs {sb:?}"
            );
            assert_eq!(
                ByteView::cmp_fast(&views[i], &heap, &views[j], &heap),
                sa.cmp(sb),
                "cmp_fast mismatch on {sa:?} vs {sb:?}"
            );
        }
    }

    drop(heap);
    pool.assert_no_leaks();
}

#[test]
fn test_decimal_and_date_arithmetic() {
    // 256-bit intermediate product: 10^22 * 10^22 / 10^20 = 10^24
    let a = 10_000_000_000_000_000_000_000_i128; // 10^22 with scale 10 => 10^12
    let prod = decimal_mul(a, 10, a, 10, 38, 0).unwrap();
    assert_eq!(prod, 1_000_000_000_000_000_000_000_000i128);

    // Exact division with half-away-from-zero rounding: 1 / 3 at scale 4 -> 0.3333, 2 / 3 -> 0.6667
    assert_eq!(decimal_div(100, 2, 300, 2, 18, 4).unwrap(), 3333);
    assert_eq!(decimal_div(200, 2, 300, 2, 18, 4).unwrap(), 6667);
    assert_eq!(decimal_div(-200, 2, 300, 2, 18, 4).unwrap(), -6667);
    assert!(matches!(
        decimal_div(10, 0, 0, 0, 18, 2),
        Err(VectraError::DivisionByZero)
    ));

    assert_eq!(format_decimal(-5, 2), "-0.05");
    assert_eq!(parse_decimal("-0.05", 10, 2).unwrap(), -5);
    assert_eq!(parse_decimal("12345.678", 18, 2).unwrap(), 1234568);

    // Date / Timestamp calendar checks
    assert_eq!(parse_date32("1970-01-01").unwrap(), 0);
    assert_eq!(
        format_date32(parse_date32("1998-12-01").unwrap() - 90),
        "1998-09-02"
    );
    assert_eq!(
        format_date32(add_months_date32(parse_date32("2024-01-31").unwrap(), 1)),
        "2024-02-29"
    );
    assert_eq!(
        format_date32(add_months_date32(parse_date32("2023-01-31").unwrap(), 1)),
        "2023-02-28"
    );
    let ts = parse_timestamp_micros("1998-12-01 23:59:59.123456").unwrap();
    assert_eq!(format_timestamp_micros(ts), "1998-12-01 23:59:59.123456");
}

#[test]
fn test_validity_mask_bitops_and_slicing() {
    let bools: Vec<bool> = (0..137).map(|i| i % 3 != 0 && i % 7 != 0).collect();
    let mask = ValidityMask::from_bools(&bools);
    assert_eq!(mask.count_valid(), bools.iter().filter(|&&b| b).count());
    let collected: Vec<usize> = mask.iter_valid_indices().collect();
    let expected: Vec<usize> = bools
        .iter()
        .enumerate()
        .filter_map(|(i, &b)| b.then_some(i))
        .collect();
    assert_eq!(collected, expected);

    // Non-64-aligned slice
    let sliced = mask.slice(19, 85);
    for i in 0..85 {
        assert_eq!(sliced.is_valid(i), bools[19 + i]);
    }

    // Round-trip bytes
    let raw = mask.to_bytes();
    let decoded = ValidityMask::from_bytes(&raw, bools.len());
    assert_eq!(decoded, mask);

    // Bitwise identities
    let inv = mask.bitnot();
    assert_eq!(mask.bitand(&inv).count_valid(), 0);
    assert!(mask.bitor(&inv).is_all_valid());
}

#[test]
fn test_exact_vector_capacity_2048_boundary() {
    let pool = BoundedMemoryPool::unbounded();
    let dyn_pool = pool.as_dyn();
    let vals: Vec<ScalarValue> = (0..VECTOR_CAPACITY)
        .map(|i| {
            if i % 17 == 0 {
                ScalarValue::Null
            } else {
                ScalarValue::Utf8(format!("row_{i:06}_payload_over_12_bytes"))
            }
        })
        .collect();
    let col = Vector::from_scalars(&vals, LogicalType::Utf8, &dyn_pool).unwrap();
    let chunk = DataChunk::new(vec![col], VECTOR_CAPACITY);
    let sel_indices: Vec<u16> = (0..VECTOR_CAPACITY as u16).rev().collect();
    let filtered = chunk.filter(&SelectionVector::from_vec(sel_indices));
    let compacted = filtered.compact(&dyn_pool).unwrap();
    assert_eq!(compacted.len(), VECTOR_CAPACITY);
    for i in 0..VECTOR_CAPACITY {
        assert_eq!(compacted.get_row(i)[0], vals[VECTOR_CAPACITY - 1 - i]);
    }
    drop(compacted);
    drop(filtered);
    drop(chunk);
    pool.assert_no_leaks();
}

#[test]
fn test_m01_10k_seed_property_fuzz() {
    // Under Miri, run 40 diverse seeds (full unsafe coverage in ~2s); natively run full 10,000 seeds!
    #[cfg(miri)]
    const SEEDS: u64 = 40;
    #[cfg(not(miri))]
    const SEEDS: u64 = 10_000;

    let all_types = [
        LogicalType::Boolean,
        LogicalType::Int8,
        LogicalType::Int16,
        LogicalType::Int32,
        LogicalType::Int64,
        LogicalType::Float64,
        LogicalType::Decimal128 {
            precision: 18,
            scale: 4,
        },
        LogicalType::Date32,
        LogicalType::TimestampMicros,
        LogicalType::Utf8,
        LogicalType::Binary,
    ];
    let all_orders = [
        SortOrder::ASC_NULLS_FIRST,
        SortOrder::ASC_NULLS_LAST,
        SortOrder::DESC_NULLS_FIRST,
        SortOrder::DESC_NULLS_LAST,
    ];

    let pool = BoundedMemoryPool::unbounded();
    let dyn_pool = pool.as_dyn();

    for seed in 0..SEEDS {
        let mut rng = SplitMix64::new(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0xA5A5_5A5A);
        let num_cols = rng.next_usize(4) + 1;
        let row_count = match rng.next_usize(10) {
            0 => 0,
            1 => 1,
            _ => rng.next_usize(24) + 1,
        };
        let types: Vec<LogicalType> = (0..num_cols).map(|_| *rng.choose(&all_types)).collect();
        let cols: Vec<Vector> = types
            .iter()
            .map(|&ty| gen_mixed_vector(&mut rng, ty, row_count, &dyn_pool, 0))
            .collect();
        let chunk = DataChunk::new(cols, row_count);

        // Chained SelectionVector filtering
        let mut expected_rows: Vec<Vec<ScalarValue>> =
            (0..row_count).map(|r| chunk.get_row(r)).collect();

        let sel1_len = if row_count == 0 {
            0
        } else {
            rng.next_usize(row_count + 1)
        };
        let sel1_idx: Vec<u16> = (0..sel1_len)
            .map(|_| rng.next_usize(row_count) as u16)
            .collect();
        let chunk1 = chunk.filter(&SelectionVector::from_vec(sel1_idx.clone()));
        expected_rows = sel1_idx
            .iter()
            .map(|&i| expected_rows[i as usize].clone())
            .collect();

        let sel2_len = if sel1_len == 0 {
            0
        } else {
            rng.next_usize(sel1_len + 1)
        };
        let sel2_idx: Vec<u16> = (0..sel2_len)
            .map(|_| rng.next_usize(sel1_len) as u16)
            .collect();
        let chunk2 = chunk1.filter(&SelectionVector::from_vec(sel2_idx.clone()));
        expected_rows = sel2_idx
            .iter()
            .map(|&i| expected_rows[i as usize].clone())
            .collect();

        // Compact & verify row-for-row
        let compacted = chunk2.compact(&dyn_pool).unwrap();
        assert_eq!(compacted.len(), expected_rows.len(), "seed {seed}");
        for (r, exp) in expected_rows.iter().enumerate() {
            assert_eq!(
                &compacted.get_row(r),
                exp,
                "compact mismatch seed {seed} row {r}"
            );
        }

        // TupleBuffer append + binary serialize/deserialize + materialize round-trip
        let mut tb = TupleBuffer::new(types.clone(), &dyn_pool).unwrap();
        let range = tb.append_chunk(&chunk2).unwrap();
        assert_eq!(range, 0..expected_rows.len());

        let mut raw = Vec::new();
        tb.serialize_to_bytes(&mut raw);
        let tb2 = TupleBuffer::deserialize_from_bytes(types.clone(), &raw, &dyn_pool).unwrap();
        let all_ids: Vec<usize> = (0..expected_rows.len()).collect();
        let restored = tb2.materialize_rows(&all_ids, &dyn_pool).unwrap();
        for (r, exp) in expected_rows.iter().enumerate() {
            assert_eq!(
                &restored.get_row(r),
                exp,
                "TupleBuffer roundtrip mismatch seed {seed} row {r}"
            );
        }

        // Composite sort-key total-order verification across random ASC/DESC x NULLS FIRST/LAST
        if expected_rows.len() >= 2 {
            let orders: Vec<SortOrder> = (0..num_cols).map(|_| *rng.choose(&all_orders)).collect();
            let mut key_a = Vec::new();
            let mut key_b = Vec::new();
            let checks = expected_rows.len().min(6);
            for i in 0..checks {
                for j in 0..checks {
                    key_a.clear();
                    key_b.clear();
                    encode_composite_sort_key(&expected_rows[i], &types, &orders, &mut key_a);
                    encode_composite_sort_key(&expected_rows[j], &types, &orders, &mut key_b);

                    let mut expected_ord = Ordering::Equal;
                    for c in 0..num_cols {
                        let o = compare_with_order(
                            &expected_rows[i][c],
                            &expected_rows[j][c],
                            orders[c],
                        );
                        if o != Ordering::Equal {
                            expected_ord = o;
                            break;
                        }
                    }
                    assert_eq!(
                        key_a.cmp(&key_b),
                        expected_ord,
                        "sort key mismatch seed {seed}: row {i}={:?} vs row {j}={:?}, orders={orders:?}, keys={key_a:02x?} vs {key_b:02x?}",
                        expected_rows[i],
                        expected_rows[j]
                    );
                }
            }
        }
    }

    pool.assert_no_leaks();
}
