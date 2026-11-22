use std::sync::Arc;
use vectra_expr::*;
use vectra_types::*;

#[test]
fn test_exhaustive_kleene_and_in_null_truth_tables() {
    let pool = BoundedMemoryPool::unbounded();
    let dyn_pool = pool.as_dyn();
    let vals = [
        ScalarValue::Boolean(true),
        ScalarValue::Boolean(false),
        ScalarValue::Null,
    ];

    let mut lhs = Vec::new();
    let mut rhs = Vec::new();
    for a in &vals {
        for b in &vals {
            lhs.push(a.clone());
            rhs.push(b.clone());
        }
    }
    let chunk = DataChunk::new(
        vec![
            Vector::from_scalars(&lhs, LogicalType::Boolean, &dyn_pool).unwrap(),
            Vector::from_scalars(&rhs, LogicalType::Boolean, &dyn_pool).unwrap(),
        ],
        lhs.len(),
    );

    let a_col = PhysicalExpr::col(0, "a", LogicalType::Boolean);
    let b_col = PhysicalExpr::col(1, "b", LogicalType::Boolean);

    for op in [
        BinaryOperator::And,
        BinaryOperator::Or,
        BinaryOperator::IsDistinctFrom,
        BinaryOperator::IsNotDistinctFrom,
        BinaryOperator::Eq,
        BinaryOperator::NotEq,
    ] {
        let expr = PhysicalExpr::binary(op, a_col.clone(), b_col.clone(), LogicalType::Boolean);
        let vec = eval_expr(&expr, &chunk, &dyn_pool).unwrap();
        for r in 0..chunk.len() {
            let expected = eval_expr_scalar_reference(&expr, &chunk.get_row(r)).unwrap();
            assert_eq!(vec.get_scalar(r), expected, "op {op:?} row {r}");
        }
    }

    // Verify IN / NOT IN with NULLs, empty lists, and >= 8 element HashSet fast-path
    let nums = vec![
        ScalarValue::Int64(1),
        ScalarValue::Int64(5),
        ScalarValue::Int64(99),
        ScalarValue::Null,
    ];
    let nchunk = DataChunk::new(
        vec![Vector::from_scalars(&nums, LogicalType::Int64, &dyn_pool).unwrap()],
        nums.len(),
    );
    let x = PhysicalExpr::col(0, "x", LogicalType::Int64);

    let lists = vec![
        vec![], // empty
        vec![
            PhysicalExpr::lit(ScalarValue::Int64(1), LogicalType::Int64),
            PhysicalExpr::lit(ScalarValue::Null, LogicalType::Int64),
        ],
        // 10 items with NULL -> triggers HashSet fast path
        vec![
            PhysicalExpr::lit(ScalarValue::Int64(1), LogicalType::Int64),
            PhysicalExpr::lit(ScalarValue::Int64(2), LogicalType::Int64),
            PhysicalExpr::lit(ScalarValue::Int64(3), LogicalType::Int64),
            PhysicalExpr::lit(ScalarValue::Int64(4), LogicalType::Int64),
            PhysicalExpr::lit(ScalarValue::Int64(6), LogicalType::Int64),
            PhysicalExpr::lit(ScalarValue::Int64(7), LogicalType::Int64),
            PhysicalExpr::lit(ScalarValue::Int64(8), LogicalType::Int64),
            PhysicalExpr::lit(ScalarValue::Int64(9), LogicalType::Int64),
            PhysicalExpr::lit(ScalarValue::Null, LogicalType::Int64),
        ],
        // 9 items without NULL -> HashSet fast path
        (1..=9)
            .map(|v| PhysicalExpr::lit(ScalarValue::Int64(v), LogicalType::Int64))
            .collect(),
    ];

    for list in lists {
        for negated in [false, true] {
            let expr = PhysicalExpr::InList {
                expr: Box::new(x.clone()),
                list: list.clone(),
                negated,
            };
            let v = eval_expr(&expr, &nchunk, &dyn_pool).unwrap();
            for r in 0..nchunk.len() {
                let exp = eval_expr_scalar_reference(&expr, &nchunk.get_row(r)).unwrap();
                assert_eq!(
                    v.get_scalar(r),
                    exp,
                    "IN list len={} neg={negated} row={r}",
                    list.len()
                );
            }
        }
    }
    drop(chunk);
    drop(nchunk);
    pool.assert_no_leaks();
}

#[test]
fn test_case_when_and_coalesce_short_circuit_selection_narrowing() {
    let pool = BoundedMemoryPool::unbounded();
    let dyn_pool = pool.as_dyn();

    let xs = vec![
        ScalarValue::Int64(0),
        ScalarValue::Int64(2),
        ScalarValue::Int64(5),
        ScalarValue::Null,
    ];
    let guards = vec![
        ScalarValue::Int64(999),
        ScalarValue::Null,
        ScalarValue::Int64(777),
        ScalarValue::Null,
    ];
    let chunk = DataChunk::new(
        vec![
            Vector::from_scalars(&xs, LogicalType::Int64, &dyn_pool).unwrap(),
            Vector::from_scalars(&guards, LogicalType::Int64, &dyn_pool).unwrap(),
        ],
        xs.len(),
    );

    let x_col = PhysicalExpr::col(0, "x", LogicalType::Int64);
    let g_col = PhysicalExpr::col(1, "g", LogicalType::Int64);
    let div_expr = PhysicalExpr::binary(
        BinaryOperator::Div,
        PhysicalExpr::lit(ScalarValue::Int64(100), LogicalType::Int64),
        x_col.clone(),
        LogicalType::Int64,
    );

    // CASE WHEN x = 0 THEN -1 ELSE 100 / x END -> must NOT divide by zero on row 0!
    let case_expr = PhysicalExpr::CaseWhen {
        branches: vec![(
            PhysicalExpr::binary(
                BinaryOperator::Eq,
                x_col.clone(),
                PhysicalExpr::lit(ScalarValue::Int64(0), LogicalType::Int64),
                LogicalType::Boolean,
            ),
            PhysicalExpr::lit(ScalarValue::Int64(-1), LogicalType::Int64),
        )],
        else_expr: Some(Box::new(div_expr.clone())),
        return_type: LogicalType::Int64,
    };
    let out = eval_expr(&case_expr, &chunk, &dyn_pool).unwrap();
    assert_eq!(out.get_scalar(0), ScalarValue::Int64(-1));
    assert_eq!(out.get_scalar(1), ScalarValue::Int64(50));
    assert_eq!(out.get_scalar(2), ScalarValue::Int64(20));
    assert_eq!(out.get_scalar(3), ScalarValue::Null);

    // COALESCE(g, 100 / x) -> row 0 has g=999 so 100/0 is never evaluated!
    let coal_expr = PhysicalExpr::Coalesce {
        args: vec![g_col, div_expr],
        return_type: LogicalType::Int64,
    };
    let out2 = eval_expr(&coal_expr, &chunk, &dyn_pool).unwrap();
    assert_eq!(out2.get_scalar(0), ScalarValue::Int64(999));
    assert_eq!(out2.get_scalar(1), ScalarValue::Int64(50));
    assert_eq!(out2.get_scalar(2), ScalarValue::Int64(777));
    assert_eq!(out2.get_scalar(3), ScalarValue::Null);

    drop(out);
    drop(out2);
    drop(chunk);
    pool.assert_no_leaks();
}

#[test]
fn test_pathological_like_linear_time() {
    let text = "a".repeat(250);
    let pat = "%a%a%a%a%a%a%a%a%a%a%a%a%b";
    assert!(!like_match(&text, pat, None, false));
    assert!(like_match(&format!("{text}b"), pat, None, false));

    // Escapes, prefix, suffix, contains, ILIKE
    assert!(like_match("foo_bar", "foo\\_bar", Some('\\'), false));
    assert!(!like_match("fooXbar", "foo\\_bar", Some('\\'), false));
    assert!(like_match("Hello World", "hello%", None, true));
    assert!(like_match("Hello World", "%WORLD", None, true));
    assert!(like_match("Hello World", "%lo wo%", None, true));
}

#[test]
fn test_simd_vs_scalar_bit_identical_parity() {
    let mut rng = SplitMix64::new(0x1234_5678_9ABC_DEF0);
    let ops = [
        CmpOp::Eq,
        CmpOp::Ne,
        CmpOp::Lt,
        CmpOp::Le,
        CmpOp::Gt,
        CmpOp::Ge,
    ];

    for &len in &[0usize, 1, 3, 4, 7, 8, 9, 15, 16, 17, 63, 64, 65, 2048] {
        let i64s: Vec<i64> = (0..len)
            .map(|k| match k % 7 {
                0 => i64::MIN,
                1 => i64::MAX,
                2 => 0,
                3 => -1,
                _ => rng.next_u64() as i64,
            })
            .collect();
        let i32s: Vec<i32> = (0..len)
            .map(|k| match k % 5 {
                0 => i32::MIN,
                1 => i32::MAX,
                2 => 0,
                _ => rng.next_u32() as i32,
            })
            .collect();
        let f64s_a: Vec<f64> = (0..len)
            .map(|k| match k % 9 {
                0 => -0.0,
                1 => 0.0,
                2 => f64::NAN,
                3 => f64::INFINITY,
                4 => f64::NEG_INFINITY,
                _ => (rng.next_f64_unit() - 0.5) * 1000.0,
            })
            .collect();
        let f64s_b: Vec<f64> = (0..len)
            .map(|_| (rng.next_f64_unit() - 0.5) * 1000.0)
            .collect();

        let mut simd_sel = Vec::new();
        let mut scalar_sel = Vec::new();

        for &op in &ops {
            for &rhs in &[0i64, -1, i64::MIN, i64::MAX, 42] {
                simd::filter_i64(&i64s, rhs, op, &mut simd_sel);
                simd::filter_i64_scalar_ref(&i64s, rhs, op, &mut scalar_sel);
                assert_eq!(
                    simd_sel, scalar_sel,
                    "i64 filter mismatch len={len} op={op:?}"
                );
            }
            for &rhs in &[0i32, i32::MIN, i32::MAX, -7] {
                simd::filter_i32(&i32s, rhs, op, &mut simd_sel);
                simd::filter_i32_scalar_ref(&i32s, rhs, op, &mut scalar_sel);
                assert_eq!(
                    simd_sel, scalar_sel,
                    "i32 filter mismatch len={len} op={op:?}"
                );
            }
            for &rhs in &[0.0f64, -0.0, f64::NAN, f64::INFINITY, 12.5] {
                simd::filter_f64(&f64s_a, rhs, op, &mut simd_sel);
                simd::filter_f64_scalar_ref(&f64s_a, rhs, op, &mut scalar_sel);
                assert_eq!(
                    simd_sel, scalar_sel,
                    "f64 filter mismatch len={len} op={op:?}"
                );
            }
        }

        let mut out_simd = vec![0.0; len];
        let mut out_scalar = vec![0.0; len];
        simd::add_f64(&f64s_a, &f64s_b, &mut out_simd);
        simd::add_f64_scalar_ref(&f64s_a, &f64s_b, &mut out_scalar);
        for i in 0..len {
            assert_eq!(
                out_simd[i].is_nan(),
                out_scalar[i].is_nan(),
                "add_f64 NaN mismatch"
            );
            if !out_simd[i].is_nan() {
                assert_eq!(out_simd[i].to_bits(), out_scalar[i].to_bits());
            }
        }
        simd::sub_f64(&f64s_a, &f64s_b, &mut out_simd);
        simd::sub_f64_scalar_ref(&f64s_a, &f64s_b, &mut out_scalar);
        for i in 0..len {
            if !out_simd[i].is_nan() {
                assert_eq!(out_simd[i].to_bits(), out_scalar[i].to_bits());
            }
        }
        simd::mul_f64(&f64s_a, &f64s_b, &mut out_simd);
        simd::mul_f64_scalar_ref(&f64s_a, &f64s_b, &mut out_scalar);
        for i in 0..len {
            if !out_simd[i].is_nan() {
                assert_eq!(out_simd[i].to_bits(), out_scalar[i].to_bits());
            }
        }

        // Checked i64 addition parity (including exact overflow detection)
        let i64s_b: Vec<i64> = (0..len)
            .map(|k| {
                if k == len.saturating_sub(1) && len > 0 {
                    1
                } else {
                    3
                }
            })
            .collect();
        let mut o_simd = vec![0i64; len];
        let mut o_scalar = vec![0i64; len];
        let r_simd = simd::add_i64_checked(&i64s, &i64s_b, &mut o_simd);
        let r_scalar = simd::add_i64_checked_scalar_ref(&i64s, &i64s_b, &mut o_scalar);
        assert_eq!(
            r_simd.is_ok(),
            r_scalar.is_ok(),
            "i64 checked add overflow parity len={len}"
        );
        if r_simd.is_ok() {
            assert_eq!(o_simd, o_scalar);
        }
    }
}

#[test]
fn test_vectorized_hashing_encoding_invariance() {
    let pool = BoundedMemoryPool::unbounded();
    let dyn_pool = pool.as_dyn();

    // -0.0 and +0.0 MUST hash identically across Flat, Constant, and Dictionary!
    let flat = Vector::from_scalars(
        &[
            ScalarValue::Float64(-0.0),
            ScalarValue::Float64(0.0),
            ScalarValue::Float64(f64::NAN),
            ScalarValue::Float64(-f64::NAN),
            ScalarValue::Null,
        ],
        LogicalType::Float64,
        &dyn_pool,
    )
    .unwrap();
    let c_neg = Vector::constant(ScalarValue::Float64(-0.0), LogicalType::Float64, 5);
    let c_pos = Vector::constant(ScalarValue::Float64(0.0), LogicalType::Float64, 5);
    let dict = Vector::dictionary(
        Arc::new(flat.clone()),
        SelectionVector::from_vec(vec![0, 1, 2, 3, 4]),
        ValidityMask::all_valid(5),
    );

    let sel = SelectionVector::identity(5);
    let mut h_flat = [0u64; 5];
    let mut h_dict = [0u64; 5];
    let mut h_cneg = [0u64; 5];
    let mut h_cpos = [0u64; 5];

    hash_vector(&flat, &sel, 5, &mut h_flat, false);
    hash_vector(&dict, &sel, 5, &mut h_dict, false);
    hash_vector(&c_neg, &sel, 5, &mut h_cneg, false);
    hash_vector(&c_pos, &sel, 5, &mut h_cpos, false);

    assert_eq!(h_flat, h_dict);
    assert_eq!(h_flat[0], h_flat[1]);
    assert_eq!(h_flat[2], h_flat[3]);
    assert_eq!(h_cneg[0], h_cpos[0]);
    assert_eq!(h_cneg[0], h_flat[0]);
}

// ============================================================================
// 50,000-Seed Differential Expression AST Fuzzer
// ============================================================================

fn gen_expr(rng: &mut SplitMix64, target_ty: LogicalType, depth: usize) -> PhysicalExpr {
    if depth == 0 || rng.gen_bool(0.25) {
        return gen_leaf(rng, target_ty);
    }

    match target_ty {
        LogicalType::Int64 => match rng.next_usize(7) {
            0 => {
                let op = *rng.choose(&[
                    BinaryOperator::Add,
                    BinaryOperator::Sub,
                    BinaryOperator::Mul,
                    BinaryOperator::Div,
                    BinaryOperator::Mod,
                ]);
                PhysicalExpr::binary(
                    op,
                    gen_expr(rng, LogicalType::Int64, depth - 1),
                    gen_expr(rng, LogicalType::Int64, depth - 1),
                    LogicalType::Int64,
                )
            }
            1 => PhysicalExpr::unary(
                UnaryOperator::Neg,
                gen_expr(rng, LogicalType::Int64, depth - 1),
                LogicalType::Int64,
            ),
            2 => PhysicalExpr::CaseWhen {
                branches: vec![
                    (
                        gen_expr(rng, LogicalType::Boolean, depth - 1),
                        gen_expr(rng, LogicalType::Int64, depth - 1),
                    ),
                    (
                        gen_expr(rng, LogicalType::Boolean, depth - 1),
                        gen_expr(rng, LogicalType::Int64, depth - 1),
                    ),
                ],
                else_expr: Some(Box::new(gen_expr(rng, LogicalType::Int64, depth - 1))),
                return_type: LogicalType::Int64,
            },
            3 => PhysicalExpr::Coalesce {
                args: vec![
                    gen_expr(rng, LogicalType::Int64, depth - 1),
                    gen_expr(rng, LogicalType::Int64, depth - 1),
                ],
                return_type: LogicalType::Int64,
            },
            4 => PhysicalExpr::ScalarFunc {
                func: ScalarFunction::Length,
                args: vec![gen_expr(rng, LogicalType::Utf8, depth - 1)],
                return_type: LogicalType::Int64,
            },
            5 => PhysicalExpr::ScalarFunc {
                func: ScalarFunction::Abs,
                args: vec![gen_expr(rng, LogicalType::Int64, depth - 1)],
                return_type: LogicalType::Int64,
            },
            _ => PhysicalExpr::Cast {
                expr: Box::new(gen_expr(rng, LogicalType::Float64, depth - 1)),
                target_type: LogicalType::Int64,
                try_cast: true,
            },
        },
        LogicalType::Float64 => match rng.next_usize(4) {
            0 => {
                let op = *rng.choose(&[
                    BinaryOperator::Add,
                    BinaryOperator::Sub,
                    BinaryOperator::Mul,
                ]);
                PhysicalExpr::binary(
                    op,
                    gen_expr(rng, LogicalType::Float64, depth - 1),
                    gen_expr(rng, LogicalType::Float64, depth - 1),
                    LogicalType::Float64,
                )
            }
            1 => PhysicalExpr::ScalarFunc {
                func: *rng.choose(&[
                    ScalarFunction::Abs,
                    ScalarFunction::Ceil,
                    ScalarFunction::Floor,
                ]),
                args: vec![gen_expr(rng, LogicalType::Float64, depth - 1)],
                return_type: LogicalType::Float64,
            },
            2 => PhysicalExpr::Coalesce {
                args: vec![
                    gen_expr(rng, LogicalType::Float64, depth - 1),
                    gen_expr(rng, LogicalType::Float64, depth - 1),
                ],
                return_type: LogicalType::Float64,
            },
            _ => PhysicalExpr::Cast {
                expr: Box::new(gen_expr(rng, LogicalType::Int64, depth - 1)),
                target_type: LogicalType::Float64,
                try_cast: false,
            },
        },
        LogicalType::Utf8 => match rng.next_usize(5) {
            0 => PhysicalExpr::ScalarFunc {
                func: *rng.choose(&[
                    ScalarFunction::Lower,
                    ScalarFunction::Upper,
                    ScalarFunction::Trim,
                ]),
                args: vec![gen_expr(rng, LogicalType::Utf8, depth - 1)],
                return_type: LogicalType::Utf8,
            },
            1 => PhysicalExpr::binary(
                BinaryOperator::Concat,
                gen_expr(rng, LogicalType::Utf8, depth - 1),
                gen_expr(rng, LogicalType::Utf8, depth - 1),
                LogicalType::Utf8,
            ),
            2 => PhysicalExpr::ScalarFunc {
                func: ScalarFunction::Substring,
                args: vec![
                    gen_expr(rng, LogicalType::Utf8, depth - 1),
                    PhysicalExpr::lit(
                        ScalarValue::Int64(rng.gen_range_i64(-2, 8)),
                        LogicalType::Int64,
                    ),
                    PhysicalExpr::lit(
                        ScalarValue::Int64(rng.gen_range_i64(0, 12)),
                        LogicalType::Int64,
                    ),
                ],
                return_type: LogicalType::Utf8,
            },
            3 => PhysicalExpr::Coalesce {
                args: vec![
                    gen_expr(rng, LogicalType::Utf8, depth - 1),
                    gen_expr(rng, LogicalType::Utf8, depth - 1),
                ],
                return_type: LogicalType::Utf8,
            },
            _ => PhysicalExpr::Cast {
                expr: Box::new(gen_expr(rng, LogicalType::Int64, depth - 1)),
                target_type: LogicalType::Utf8,
                try_cast: false,
            },
        },
        LogicalType::Boolean => match rng.next_usize(8) {
            0 => {
                let op = *rng.choose(&[BinaryOperator::And, BinaryOperator::Or]);
                PhysicalExpr::binary(
                    op,
                    gen_expr(rng, LogicalType::Boolean, depth - 1),
                    gen_expr(rng, LogicalType::Boolean, depth - 1),
                    LogicalType::Boolean,
                )
            }
            1 => PhysicalExpr::unary(
                *rng.choose(&[
                    UnaryOperator::Not,
                    UnaryOperator::IsNull,
                    UnaryOperator::IsNotNull,
                ]),
                gen_expr(rng, LogicalType::Boolean, depth - 1),
                LogicalType::Boolean,
            ),
            2 => {
                let cmp_ty =
                    *rng.choose(&[LogicalType::Int64, LogicalType::Float64, LogicalType::Utf8]);
                let op = *rng.choose(&[
                    BinaryOperator::Eq,
                    BinaryOperator::NotEq,
                    BinaryOperator::Lt,
                    BinaryOperator::LtEq,
                    BinaryOperator::Gt,
                    BinaryOperator::GtEq,
                    BinaryOperator::IsDistinctFrom,
                    BinaryOperator::IsNotDistinctFrom,
                ]);
                PhysicalExpr::binary(
                    op,
                    gen_expr(rng, cmp_ty, depth - 1),
                    gen_expr(rng, cmp_ty, depth - 1),
                    LogicalType::Boolean,
                )
            }
            3 => PhysicalExpr::Between {
                expr: Box::new(gen_expr(rng, LogicalType::Int64, depth - 1)),
                low: Box::new(gen_expr(rng, LogicalType::Int64, depth - 1)),
                high: Box::new(gen_expr(rng, LogicalType::Int64, depth - 1)),
                negated: rng.gen_bool(0.5),
            },
            4 => {
                let pat = *rng.choose(&["%a%", "abc%", "%xyz", "a_b%", "%a%a%b", "exact"]);
                PhysicalExpr::Like {
                    expr: Box::new(gen_expr(rng, LogicalType::Utf8, depth - 1)),
                    pattern: Box::new(PhysicalExpr::lit(
                        ScalarValue::Utf8(pat.to_string()),
                        LogicalType::Utf8,
                    )),
                    escape: None,
                    case_insensitive: rng.gen_bool(0.5),
                    negated: rng.gen_bool(0.5),
                }
            }
            5 => {
                let list_len = if rng.gen_bool(0.3) {
                    10
                } else {
                    rng.next_usize(4)
                };
                let list: Vec<PhysicalExpr> = (0..list_len)
                    .map(|_| gen_leaf(rng, LogicalType::Int64))
                    .collect();
                PhysicalExpr::InList {
                    expr: Box::new(gen_expr(rng, LogicalType::Int64, depth - 1)),
                    list,
                    negated: rng.gen_bool(0.5),
                }
            }
            6 => PhysicalExpr::CaseWhen {
                branches: vec![(
                    gen_expr(rng, LogicalType::Boolean, depth - 1),
                    gen_expr(rng, LogicalType::Boolean, depth - 1),
                )],
                else_expr: Some(Box::new(gen_expr(rng, LogicalType::Boolean, depth - 1))),
                return_type: LogicalType::Boolean,
            },
            _ => PhysicalExpr::unary(
                *rng.choose(&[UnaryOperator::IsNull, UnaryOperator::IsNotNull]),
                gen_expr(rng, LogicalType::Int64, depth - 1),
                LogicalType::Boolean,
            ),
        },
        _ => gen_leaf(rng, target_ty),
    }
}

fn gen_leaf(rng: &mut SplitMix64, ty: LogicalType) -> PhysicalExpr {
    match ty {
        LogicalType::Int64 => match rng.next_usize(5) {
            0 => PhysicalExpr::col(0, "i0", LogicalType::Int64),
            1 => PhysicalExpr::col(1, "i1", LogicalType::Int64),
            2 => PhysicalExpr::lit(ScalarValue::Null, LogicalType::Int64),
            _ => PhysicalExpr::lit(
                ScalarValue::Int64(rng.gen_range_i64(-10, 10)),
                LogicalType::Int64,
            ),
        },
        LogicalType::Float64 => match rng.next_usize(4) {
            0 => PhysicalExpr::col(2, "f0", LogicalType::Float64),
            1 => PhysicalExpr::lit(ScalarValue::Null, LogicalType::Float64),
            2 => PhysicalExpr::lit(ScalarValue::Float64(-0.0), LogicalType::Float64),
            _ => PhysicalExpr::lit(
                ScalarValue::Float64(rng.gen_range_i64(-5, 5) as f64 * 0.5),
                LogicalType::Float64,
            ),
        },
        LogicalType::Utf8 => match rng.next_usize(4) {
            0 => PhysicalExpr::col(3, "s0", LogicalType::Utf8),
            1 => PhysicalExpr::col(4, "s1", LogicalType::Utf8),
            2 => PhysicalExpr::lit(ScalarValue::Null, LogicalType::Utf8),
            _ => PhysicalExpr::lit(
                ScalarValue::Utf8(
                    (*rng.choose(&[
                        "",
                        "abc",
                        "abc_longer_than_12_bytes_xyz",
                        "aXb_tail",
                        "exact",
                        "HELLO",
                    ]))
                    .to_string(),
                ),
                LogicalType::Utf8,
            ),
        },
        LogicalType::Boolean => match rng.next_usize(4) {
            0 => PhysicalExpr::col(5, "b0", LogicalType::Boolean),
            1 => PhysicalExpr::lit(ScalarValue::Boolean(true), LogicalType::Boolean),
            2 => PhysicalExpr::lit(ScalarValue::Boolean(false), LogicalType::Boolean),
            _ => PhysicalExpr::lit(ScalarValue::Null, LogicalType::Boolean),
        },
        _ => PhysicalExpr::lit(ScalarValue::Null, ty),
    }
}

#[test]
fn test_m02_50k_differential_expression_fuzzer() {
    #[cfg(miri)]
    const SEEDS: u64 = 100;
    #[cfg(not(miri))]
    const SEEDS: u64 = 50_000;

    let pool = BoundedMemoryPool::unbounded();
    let dyn_pool = pool.as_dyn();

    // Build adversarial mixed-encoding DataChunk (Flat, Constant, Dictionary + SelectionVector)
    let base_i0 = vec![
        ScalarValue::Int64(0),
        ScalarValue::Int64(1),
        ScalarValue::Int64(-1),
        ScalarValue::Int64(7),
        ScalarValue::Int64(100),
        ScalarValue::Null,
        ScalarValue::Int64(-42),
        ScalarValue::Int64(9),
    ];
    let base_i1 = vec![
        ScalarValue::Int64(3),
        ScalarValue::Int64(-5),
        ScalarValue::Int64(2),
        ScalarValue::Null,
        ScalarValue::Int64(4),
        ScalarValue::Int64(1),
        ScalarValue::Int64(2),
        ScalarValue::Int64(-1),
    ];
    let base_f0 = vec![
        ScalarValue::Float64(-0.0),
        ScalarValue::Float64(0.0),
        ScalarValue::Float64(f64::NAN),
        ScalarValue::Float64(1.5),
        ScalarValue::Float64(-3.25),
        ScalarValue::Null,
        ScalarValue::Float64(42.0),
        ScalarValue::Float64(-100.5),
    ];
    let long_utf8 = format!("{}🚀_long_payload_abc_xyz", "a".repeat(105));
    let base_s0 = vec![
        ScalarValue::Utf8(String::new()),
        ScalarValue::Utf8("abc".into()),
        ScalarValue::Utf8("abc_longer_than_12_bytes_xyz".into()),
        ScalarValue::Utf8(long_utf8),
        ScalarValue::Null,
        ScalarValue::Utf8("EXACT".into()),
        ScalarValue::Utf8("  trimmed  ".into()),
        ScalarValue::Utf8("aXb_tail".into()),
    ];
    let base_b0 = vec![
        ScalarValue::Boolean(true),
        ScalarValue::Boolean(false),
        ScalarValue::Null,
        ScalarValue::Boolean(true),
        ScalarValue::Boolean(false),
        ScalarValue::Null,
        ScalarValue::Boolean(true),
        ScalarValue::Boolean(false),
    ];

    let flat_i0 = Vector::from_scalars(&base_i0, LogicalType::Int64, &dyn_pool).unwrap();
    let dict_i1 = Vector::dictionary(
        Arc::new(Vector::from_scalars(&base_i1, LogicalType::Int64, &dyn_pool).unwrap()),
        SelectionVector::from_vec(vec![0, 1, 2, 3, 4, 5, 6, 7]),
        ValidityMask::from_bools(&[true, true, true, true, false, true, true, true]),
    );
    let flat_f0 = Vector::from_scalars(&base_f0, LogicalType::Float64, &dyn_pool).unwrap();
    let flat_s0 = Vector::from_scalars(&base_s0, LogicalType::Utf8, &dyn_pool).unwrap();
    let const_s1 = Vector::constant(
        ScalarValue::Utf8("abc_const_val_over12".into()),
        LogicalType::Utf8,
        8,
    );
    let flat_b0 = Vector::from_scalars(&base_b0, LogicalType::Boolean, &dyn_pool).unwrap();

    let base_chunk = DataChunk::new(
        vec![flat_i0, dict_i1, flat_f0, flat_s0, const_s1, flat_b0],
        8,
    );
    let filtered_chunk = base_chunk.filter(&SelectionVector::from_vec(vec![7, 5, 2, 0, 3, 1]));

    let root_types = [
        LogicalType::Boolean,
        LogicalType::Int64,
        LogicalType::Float64,
        LogicalType::Utf8,
    ];

    for seed in 0..SEEDS {
        let mut rng = SplitMix64::new(seed.wrapping_mul(0xBF58_476D_1CE4_E5B9) ^ 0x5555_AAAA);
        let depth = rng.next_usize(5) + 1;
        let root_ty = *rng.choose(&root_types);
        let expr = gen_expr(&mut rng, root_ty, depth);
        let chunk = if seed % 2 == 0 {
            &base_chunk
        } else {
            &filtered_chunk
        };

        // Reference evaluation row by row
        let mut ref_rows = Vec::with_capacity(chunk.len());
        let mut ref_err = false;
        for r in 0..chunk.len() {
            match eval_expr_scalar_reference(&expr, &chunk.get_row(r)) {
                Ok(v) => ref_rows.push(v),
                Err(_) => {
                    ref_err = true;
                    break;
                }
            }
        }

        // Vectorized evaluation
        let vec_res = eval_expr(&expr, chunk, &dyn_pool);
        assert_eq!(
            vec_res.is_err(),
            ref_err,
            "seed {seed}: error parity mismatch! expr={expr:?}, vec={vec_res:?}, ref_err={ref_err}"
        );

        if let Ok(vec) = vec_res {
            for (r, expected) in ref_rows.iter().enumerate() {
                let actual = vec.get_scalar(r);
                assert_eq!(
                    actual, *expected,
                    "seed {seed} row {r}: value mismatch! expr={expr:?}"
                );
            }

            // For boolean expressions, also verify eval_filter agrees 100% with ref_rows!
            if root_ty == LogicalType::Boolean {
                let sel = eval_filter(&expr, chunk, &dyn_pool).unwrap();
                let expected_idx: Vec<usize> = ref_rows
                    .iter()
                    .enumerate()
                    .filter_map(|(i, v)| matches!(v, ScalarValue::Boolean(true)).then_some(i))
                    .collect();
                let actual_idx: Vec<usize> = sel.iter().collect();
                assert_eq!(
                    actual_idx, expected_idx,
                    "seed {seed}: eval_filter mismatch! expr={expr:?}"
                );
            }
        }
    }

    drop(filtered_chunk);
    drop(base_chunk);
    pool.assert_no_leaks();
}
