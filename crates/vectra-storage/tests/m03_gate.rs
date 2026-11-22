use std::sync::Arc;
use std::thread;
use vectra_storage::*;
use vectra_types::*;

#[test]
fn test_m03_1000_seed_codec_roundtrip_fuzz() {
    let pool = BoundedMemoryPool::unbounded();
    let dyn_pool = pool.as_dyn();
    let codecs = [
        CodecId::Uncompressed,
        CodecId::FrameOfReferenceBitPack,
        CodecId::DeltaBinaryPacked,
        CodecId::Rle,
        CodecId::DictionaryLz4,
    ];
    let types = [
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

    for seed in 0..1_000u64 {
        let mut rng = SplitMix64::new(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0xCAFE_BABE);
        let ty = *rng.choose(&types);
        let dist = seed % 6; // 0: empty/tiny, 1: constant, 2: monotonic, 3: zipf/low-card, 4: 99% NULL, 5: high-entropy
        let len = match dist {
            0 => rng.next_usize(3),
            _ => rng.next_usize(180) + 1,
        };

        let mut vals = Vec::with_capacity(len);
        for i in 0..len {
            let is_null = match dist {
                4 => rng.gen_bool(0.99),
                1 => false,
                _ => rng.gen_bool(0.15),
            };
            if is_null {
                vals.push(ScalarValue::Null);
                continue;
            }
            let v = match ty {
                LogicalType::Boolean => {
                    ScalarValue::Boolean(if dist == 1 { true } else { rng.gen_bool(0.5) })
                }
                LogicalType::Int8 => ScalarValue::Int8(if dist == 1 {
                    7
                } else if dist == 2 {
                    (i % 120) as i8
                } else {
                    rng.next_u32() as i8
                }),
                LogicalType::Int16 => ScalarValue::Int16(if dist == 1 {
                    42
                } else if dist == 2 {
                    (i * 3) as i16
                } else {
                    rng.next_u32() as i16
                }),
                LogicalType::Int32 => ScalarValue::Int32(if dist == 1 {
                    999
                } else if dist == 2 {
                    10_000 + (i as i32) * 5
                } else if dist == 3 {
                    rng.gen_range_i64(0, 4) as i32
                } else {
                    rng.next_u32() as i32
                }),
                LogicalType::Int64 => ScalarValue::Int64(if dist == 1 {
                    -123456789
                } else if dist == 2 {
                    1_000_000 + (i as i64) * 17
                } else if dist == 3 {
                    rng.gen_range_i64(-3, 3)
                } else {
                    rng.next_u64() as i64
                }),
                LogicalType::Float64 => ScalarValue::Float64(if dist == 1 {
                    2.75_f64
                } else if dist == 3 {
                    (rng.next_usize(4) as f64) * 2.5
                } else {
                    (rng.next_f64_unit() - 0.5) * 1e9
                }),
                LogicalType::Decimal128 { precision, scale } => ScalarValue::Decimal128 {
                    value: if dist == 1 {
                        123456
                    } else {
                        rng.gen_range_i64(-1_000_000, 1_000_000) as i128
                    },
                    precision,
                    scale,
                },
                LogicalType::Date32 => ScalarValue::Date32(if dist == 2 {
                    10_000 + i as i32
                } else {
                    rng.gen_range_i64(0, 20_000) as i32
                }),
                LogicalType::TimestampMicros => ScalarValue::TimestampMicros(if dist == 2 {
                    1_700_000_000_000_000 + (i as i64) * 1_000
                } else {
                    rng.gen_range_i64(0, 2_000_000_000_000)
                }),
                LogicalType::Utf8 => {
                    let s = match dist {
                        1 => "constant_repeated_string_payload".to_string(),
                        3 => (*rng.choose(&["URGENT", "HIGH", "MEDIUM", "LOW"])).to_string(),
                        _ => format!("val_{}_{}_payload_text", seed, rng.next_u32()),
                    };
                    ScalarValue::Utf8(s)
                }
                LogicalType::Binary => {
                    let b = match dist {
                        1 => vec![0xDE, 0xAD, 0xBE, 0xEF],
                        3 => vec![rng.next_usize(3) as u8; 8],
                        _ => (0..rng.next_usize(24))
                            .map(|_| rng.next_u32() as u8)
                            .collect(),
                    };
                    ScalarValue::Binary(b)
                }
                LogicalType::Null => ScalarValue::Null,
            };
            vals.push(v);
        }

        let flat = FlatVector::from_scalars(&vals, ty, &dyn_pool).unwrap();
        for &codec in &codecs {
            let page = encode_page(&flat, codec).unwrap();
            let decoded = decode_page(&page.raw_bytes, &dyn_pool).unwrap();
            assert_eq!(decoded.len(), len, "seed {seed} codec {codec:?} ty {ty}");
            for (r, expected) in vals.iter().enumerate() {
                assert_eq!(
                    decoded.get_scalar(r),
                    *expected,
                    "roundtrip mismatch seed {seed} codec {codec:?} ty {ty} row {r}"
                );
            }
        }
        // Also verify automatic CodecSelector
        let auto_page = encode_page_auto(&flat).unwrap();
        let decoded_auto = decode_page(&auto_page.raw_bytes, &dyn_pool).unwrap();
        for (r, expected) in vals.iter().enumerate() {
            assert_eq!(decoded_auto.get_scalar(r), *expected);
        }
    }
    pool.assert_no_leaks();
}

#[test]
fn test_corruption_resilience_crc32c_and_vdb_double_buffer() {
    let pool = BoundedMemoryPool::unbounded();
    let dyn_pool = pool.as_dyn();

    // 1. Page-level CRC32C bit-flip resilience
    let vals: Vec<ScalarValue> = (0..500).map(ScalarValue::Int64).collect();
    let flat = FlatVector::from_scalars(&vals, LogicalType::Int64, &dyn_pool).unwrap();
    let page = encode_page_auto(&flat).unwrap();

    for byte_pos in [0, 5, 16, 24, 32, page.raw_bytes.len() - 1] {
        let mut corrupted = page.raw_bytes.clone();
        corrupted[byte_pos] ^= 0x04;
        let err = decode_page(&corrupted, &dyn_pool).unwrap_err();
        assert!(
            matches!(
                err,
                VectraError::ChecksumMismatch { .. } | VectraError::CorruptFormat(_)
            ),
            "expected checksum/format error at byte {byte_pos}, got {err:?}"
        );
    }

    // 2. .vdb file double-buffered ping-pong persistence + corruption resilience
    let catalog = Catalog::new();
    let schema = Schema::new(vec![
        Field::new("id", LogicalType::Int64, false),
        Field::new("name", LogicalType::Utf8, true),
    ]);
    let tbl = catalog
        .create_table("users", schema.clone(), false)
        .unwrap();
    let s1 = catalog.begin_txn();
    let chunk = DataChunk::new(
        vec![
            Vector::from_scalars(
                &[ScalarValue::Int64(1), ScalarValue::Int64(2)],
                LogicalType::Int64,
                &dyn_pool,
            )
            .unwrap(),
            Vector::from_scalars(
                &[
                    ScalarValue::Utf8("alice".into()),
                    ScalarValue::Utf8("bob_long_username_12345".into()),
                ],
                LogicalType::Utf8,
                &dyn_pool,
            )
            .unwrap(),
        ],
        2,
    );
    tbl.insert_chunk(&chunk, s1, &dyn_pool).unwrap();
    catalog.commit_txn(s1.txn_id).unwrap();

    let tmp_path = std::env::temp_dir().join("vectradb_m03_test.vdb");
    let _ = std::fs::remove_file(&tmp_path);

    let snap_read = catalog.begin_txn();
    let gen1 = catalog.save_to_file(&tmp_path, snap_read).unwrap();
    assert_eq!(gen1, 1);
    let gen2 = catalog.save_to_file(&tmp_path, snap_read).unwrap();
    assert_eq!(gen2, 2);

    // Reload cleanly
    let loaded = Catalog::load_from_file(&tmp_path).unwrap();
    let lt = loaded.get_table("users").unwrap();
    let lsnap = loaded.begin_txn();
    let (chunks, _) = lt
        .scan(None, None, loaded.transaction_manager(), lsnap, &dyn_pool)
        .unwrap();
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        chunks[0].get_row(1)[1],
        ScalarValue::Utf8("bob_long_username_12345".into())
    );

    // Corrupt payload byte -> MUST return ChecksumMismatch
    let mut file_bytes = std::fs::read(&tmp_path).unwrap();
    let last = file_bytes.len() - 5;
    file_bytes[last] ^= 0xFF;
    let err = deserialize_vdb_bytes(&file_bytes).unwrap_err();
    assert!(matches!(err, VectraError::ChecksumMismatch { .. }));

    // Corrupt ONE superblock (slot 1 / gen 2) -> falls back cleanly to valid superblock slot 0 (gen 1)!
    let mut sb_corrupt = std::fs::read(&tmp_path).unwrap();
    sb_corrupt[16 + 64 + 2] ^= 0xAA;
    let (_, recovered_gen) = deserialize_vdb_bytes(&sb_corrupt).unwrap();
    assert_eq!(recovered_gen, 1);

    let _ = std::fs::remove_file(&tmp_path);
}

#[test]
fn test_zonemap_and_bloom_pruning_1m_rows() {
    let pool = BoundedMemoryPool::unbounded();
    let dyn_pool = pool.as_dyn();

    // 1,000,000 sorted rows across 16 RowGroups (~489 sub-blocks)
    const TOTAL_ROWS: usize = 1_000_000;
    let schema = Schema::new(vec![
        Field::new("ts", LogicalType::TimestampMicros, false),
        Field::new("order_id", LogicalType::Int64, false),
    ]);
    let catalog = Catalog::new();
    let tbl = catalog.create_table("events", schema, false).unwrap();
    let snap = catalog.begin_txn();

    let mut row = 0usize;
    while row < TOTAL_ROWS {
        let take = (TOTAL_ROWS - row).min(ROW_GROUP_SIZE);
        let ts_vals: Vec<i64> = (row..row + take)
            .map(|i| 1_700_000_000_000_000 + (i as i64) * 1_000)
            .collect();
        let id_vals: Vec<i64> = (row..row + take).map(|i| (i as i64) * 3 + 7).collect();
        let ts_vec = Vector::Flat(FlatVector {
            logical_type: LogicalType::TimestampMicros,
            data: AlignedBuffer::from_typed_slice(&ts_vals, &dyn_pool).unwrap(),
            validity: ValidityMask::all_valid(take),
            string_heap: Vec::new(),
            len: take,
        });
        let id_vec = Vector::Flat(FlatVector::from_i64_slice(&id_vals, &dyn_pool).unwrap());
        let chunk = DataChunk::new(vec![ts_vec, id_vec], take);
        tbl.insert_chunk(&chunk, snap, &dyn_pool).unwrap();
        row += take;
    }
    catalog.commit_txn(snap.txn_id).unwrap();

    let read_snap = catalog.begin_txn();
    // Selective range query: 100 rows around index 500,000 -> must skip > 98% of sub-blocks!
    let low = ScalarValue::TimestampMicros(1_700_000_000_000_000 + 500_000 * 1_000);
    let high = ScalarValue::TimestampMicros(1_700_000_000_000_000 + 500_099 * 1_000);
    let pred = PruningPredicate::Between(0, low, high);
    let (_, tel) = tbl
        .scan(
            Some(&[0, 1]),
            Some(&pred),
            catalog.transaction_manager(),
            read_snap,
            &dyn_pool,
        )
        .unwrap();
    assert!(
        tel.block_skip_ratio() > 0.98,
        "expected >98% block skip, got {:.2}% ({}/{})",
        tel.block_skip_ratio() * 100.0,
        tel.sub_blocks_pruned,
        tel.sub_blocks_total
    );

    // Point query on order_id = 3 * 750_000 + 7 -> skips > 99% of sub-blocks!
    let point_pred = PruningPredicate::Eq(1, ScalarValue::Int64(750_000 * 3 + 7));
    let (_, ptel) = tbl
        .scan(
            Some(&[1]),
            Some(&point_pred),
            catalog.transaction_manager(),
            read_snap,
            &dyn_pool,
        )
        .unwrap();
    assert!(ptel.block_skip_ratio() > 0.99);

    // Bloom filter FPR check: insert 10,000 keys, probe 20,000 absent keys -> FPR < 1.5%
    let mut bf = SplitBlockBloomFilter::with_capacity(10_000);
    for k in 0..10_000i64 {
        bf.insert_scalar(&ScalarValue::Int64(k));
    }
    for k in 0..10_000i64 {
        assert!(bf.might_contain_scalar(&ScalarValue::Int64(k))); // zero false negatives!
    }
    let mut fp = 0usize;
    for k in 1_000_000..1_020_000i64 {
        if bf.might_contain_scalar(&ScalarValue::Int64(k)) {
            fp += 1;
        }
    }
    let fpr = (fp as f64) / 20_000.0;
    assert!(fpr < 0.015, "Bloom FPR {:.4} >= 1.5%", fpr);
}

#[test]
fn test_mvcc_snapshot_isolation_and_8_thread_concurrency() {
    let pool = BoundedMemoryPool::unbounded();
    let dyn_pool = pool.as_dyn();
    let catalog = Arc::new(Catalog::new());
    let schema = Schema::new(vec![Field::new("k", LogicalType::Int64, false)]);
    let tbl = catalog.create_table("accounts", schema, false).unwrap();

    // Seed initial committed rows 0..100
    let init_snap = catalog.begin_txn();
    let init_vals: Vec<ScalarValue> = (0..100).map(ScalarValue::Int64).collect();
    let init_chunk = DataChunk::new(
        vec![Vector::from_scalars(&init_vals, LogicalType::Int64, &dyn_pool).unwrap()],
        100,
    );
    tbl.insert_chunk(&init_chunk, init_snap, &dyn_pool).unwrap();
    catalog.commit_txn(init_snap.txn_id).unwrap();

    // Snapshot isolation check: reader begins before uncommitted insert & rollback
    let reader_snap = catalog.begin_txn();
    let writer_abort = catalog.begin_txn();
    let extra = DataChunk::new(
        vec![
            Vector::from_scalars(&[ScalarValue::Int64(999)], LogicalType::Int64, &dyn_pool)
                .unwrap(),
        ],
        1,
    );
    tbl.insert_chunk(&extra, writer_abort, &dyn_pool).unwrap();
    // Writer sees its own insert (101), reader sees snapshot (100)!
    assert_eq!(
        tbl.visible_row_count(catalog.transaction_manager(), writer_abort),
        101
    );
    assert_eq!(
        tbl.visible_row_count(catalog.transaction_manager(), reader_snap),
        100
    );
    catalog.rollback_txn(writer_abort.txn_id).unwrap();
    let after_abort = catalog.begin_txn();
    assert_eq!(
        tbl.visible_row_count(catalog.transaction_manager(), after_abort),
        100
    );

    // Deterministic Write-Write conflict test
    let t1 = catalog.begin_txn();
    let t2 = catalog.begin_txn();
    let d1 = tbl
        .delete_where(
            |row| Ok(row[0] == ScalarValue::Int64(5)),
            catalog.transaction_manager(),
            t1,
            &dyn_pool,
        )
        .unwrap();
    assert_eq!(d1, 1);
    // t2 trying to delete the same row while t1 is active -> TransactionConflict!
    let conflict = tbl.delete_where(
        |row| Ok(row[0] == ScalarValue::Int64(5)),
        catalog.transaction_manager(),
        t2,
        &dyn_pool,
    );
    assert!(matches!(conflict, Err(VectraError::TransactionConflict(_))));
    catalog.commit_txn(t1.txn_id).unwrap();
    // Even after t1 commits, t2's snapshot started BEFORE t1 committed -> still Write-Write conflict!
    let conflict2 = tbl.delete_where(
        |row| Ok(row[0] == ScalarValue::Int64(5)),
        catalog.transaction_manager(),
        t2,
        &dyn_pool,
    );
    assert!(matches!(
        conflict2,
        Err(VectraError::TransactionConflict(_))
    ));
    catalog.rollback_txn(t2.txn_id).unwrap();

    // 8 concurrent threads (4 readers + 4 writers inserting/rolling back)
    let mut handles = Vec::new();
    for tid in 0..8usize {
        let cat = Arc::clone(&catalog);
        let t = Arc::clone(&tbl);
        let p = Arc::clone(&dyn_pool);
        handles.push(thread::spawn(move || {
            for iter in 0..25usize {
                if tid % 2 == 0 {
                    // Reader: verify snapshot consistency (two reads under same snapshot match identically)
                    let s = cat.begin_txn();
                    let c1 = t.visible_row_count(cat.transaction_manager(), s);
                    let c2 = t.visible_row_count(cat.transaction_manager(), s);
                    assert_eq!(c1, c2, "snapshot phantom/non-repeatable read anomaly!");
                } else {
                    // Writer: insert 2 rows; commit even iters, rollback odd iters
                    let s = cat.begin_txn();
                    let base = (tid * 10_000 + iter * 10) as i64;
                    let chunk = DataChunk::new(
                        vec![Vector::from_scalars(
                            &[ScalarValue::Int64(base), ScalarValue::Int64(base + 1)],
                            LogicalType::Int64,
                            &p,
                        )
                        .unwrap()],
                        2,
                    );
                    t.insert_chunk(&chunk, s, &p).unwrap();
                    if iter % 2 == 0 {
                        cat.commit_txn(s.txn_id).unwrap();
                    } else {
                        cat.rollback_txn(s.txn_id).unwrap();
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    // Final count: 100 initial - 1 deleted + 4 writer threads * 13 committed iters * 2 rows = 203!
    let final_snap = catalog.begin_txn();
    assert_eq!(
        tbl.visible_row_count(catalog.transaction_manager(), final_snap),
        203
    );
}

#[test]
fn test_native_parquet_v2_roundtrip_and_statistics_pruning() {
    let pool = BoundedMemoryPool::unbounded();
    let dyn_pool = pool.as_dyn();

    let schema = Schema::new(vec![
        Field::new("id", LogicalType::Int64, true),
        Field::new("flag", LogicalType::Boolean, true),
        Field::new(
            "price",
            LogicalType::Decimal128 {
                precision: 12,
                scale: 2,
            },
            true,
        ),
        Field::new("dt", LogicalType::Date32, true),
        Field::new("score", LogicalType::Float64, true),
        Field::new("region", LogicalType::Utf8, true),
    ]);

    let make_chunk = |base: i64| -> DataChunk {
        let n = 64usize;
        let ids: Vec<ScalarValue> = (0..n)
            .map(|i| ScalarValue::Int64(base + i as i64))
            .collect();
        let flags: Vec<ScalarValue> = (0..n)
            .map(|i| {
                if i % 7 == 0 {
                    ScalarValue::Null
                } else {
                    ScalarValue::Boolean(i % 2 == 0)
                }
            })
            .collect();
        let prices: Vec<ScalarValue> = (0..n)
            .map(|i| {
                if i % 11 == 0 {
                    ScalarValue::Null
                } else {
                    ScalarValue::Decimal128 {
                        value: (base + i as i64) as i128 * 105 - 5000,
                        precision: 12,
                        scale: 2,
                    }
                }
            })
            .collect();
        let dates: Vec<ScalarValue> = (0..n)
            .map(|i| ScalarValue::Date32(10_000 + (base as i32) + i as i32))
            .collect();
        let scores: Vec<ScalarValue> = (0..n)
            .map(|i| ScalarValue::Float64(i as f64 * 1.25))
            .collect();
        let regions: Vec<ScalarValue> = (0..n)
            .map(|i| {
                if i % 9 == 0 {
                    ScalarValue::Null
                } else {
                    ScalarValue::Utf8(["AMERICA", "ASIA", "EUROPE", "AFRICA"][i % 4].to_string())
                }
            })
            .collect();
        DataChunk::new(
            vec![
                Vector::from_scalars(&ids, LogicalType::Int64, &dyn_pool).unwrap(),
                Vector::from_scalars(&flags, LogicalType::Boolean, &dyn_pool).unwrap(),
                Vector::from_scalars(
                    &prices,
                    LogicalType::Decimal128 {
                        precision: 12,
                        scale: 2,
                    },
                    &dyn_pool,
                )
                .unwrap(),
                Vector::from_scalars(&dates, LogicalType::Date32, &dyn_pool).unwrap(),
                Vector::from_scalars(&scores, LogicalType::Float64, &dyn_pool).unwrap(),
                Vector::from_scalars(&regions, LogicalType::Utf8, &dyn_pool).unwrap(),
            ],
            n,
        )
    };

    let chunks = vec![
        make_chunk(0),
        make_chunk(1000),
        make_chunk(2000),
        make_chunk(3000),
    ];

    for compression in [
        ParquetCompression::Uncompressed,
        ParquetCompression::Snappy,
        ParquetCompression::Lz4Raw,
    ] {
        for use_dict in [false, true] {
            let parquet_bytes =
                write_parquet_bytes(&schema, &chunks, compression, use_dict).unwrap();

            // Full round-trip verification
            let (out_schema, read_chunks, stats) =
                read_parquet_bytes(&parquet_bytes, None, None, &dyn_pool).unwrap();
            assert_eq!(out_schema.len(), schema.len());
            assert_eq!(stats.row_groups_total, 4);
            assert_eq!(stats.row_groups_pruned, 0);
            assert_eq!(read_chunks.len(), 4);
            for (orig, got) in chunks.iter().zip(read_chunks.iter()) {
                assert_eq!(orig.len(), got.len());
                for r in 0..orig.len() {
                    assert_eq!(orig.get_row(r), got.get_row(r));
                }
            }

            // Statistics pruning verification: id BETWEEN 2010 AND 2020 must prune 3 of 4 RowGroups!
            let pred =
                PruningPredicate::Between(0, ScalarValue::Int64(2010), ScalarValue::Int64(2020));
            let (_, pruned_chunks, pstats) =
                read_parquet_bytes(&parquet_bytes, Some(&[0, 5]), Some(&pred), &dyn_pool).unwrap();
            assert_eq!(pstats.row_groups_total, 4);
            assert_eq!(pstats.row_groups_pruned, 3);
            assert_eq!(pruned_chunks.len(), 1);
            assert_eq!(pruned_chunks[0].num_columns(), 2);
        }
    }
}

#[test]
fn test_csv_rfc4180_and_inference() {
    let pool = BoundedMemoryPool::unbounded();
    let dyn_pool = pool.as_dyn();
    let csv = "id,name,score,active,joined\n\
               1,\"Smith, \"\"Jr\"\"\",98.5,true,2024-03-15\n\
               2,Alice,,false,2024-04-01\n\
               3,NULL,42.0,true,2024-05-20\n";
    let (schema, chunks) = load_csv_chunks(csv, None, &CsvOptions::default(), &dyn_pool).unwrap();
    assert_eq!(schema.field(0).logical_type, LogicalType::Int64);
    assert_eq!(schema.field(1).logical_type, LogicalType::Utf8);
    assert_eq!(schema.field(2).logical_type, LogicalType::Float64);
    assert_eq!(schema.field(3).logical_type, LogicalType::Boolean);
    assert_eq!(schema.field(4).logical_type, LogicalType::Date32);
    assert_eq!(chunks.len(), 1);
    assert_eq!(
        chunks[0].get_row(0)[1],
        ScalarValue::Utf8("Smith, \"Jr\"".into())
    );
    assert_eq!(chunks[0].get_row(1)[2], ScalarValue::Null);
    assert_eq!(chunks[0].get_row(2)[1], ScalarValue::Null);
}
