use crate::cast::cast_scalar;
use crate::expr::{BinaryOperator, PhysicalExpr, UnaryOperator};
use crate::functions::eval_scalar_function;
use crate::kleene::{eval_kleene_and, eval_kleene_not, eval_kleene_or};
use crate::like::{like_match, CompiledLike};
use crate::simd::{self, CmpOp};
use std::collections::HashSet;
use std::sync::Arc;
use vectra_types::{
    decimal_add, decimal_div, decimal_mul, decimal_rescale, decimal_sub, AlignedBufferMut,
    ByteView, DataChunk, FlatVector, LogicalType, MemoryPool, Result, ScalarValue, SelectionVector,
    ValidityMask, ValidityMaskBuilder, Vector, VectraError, MAX_DECIMAL_PRECISION,
};

/// Evaluates `expr` over the active logical rows (`0..chunk.len()`) of `chunk`,
/// returning a `Vector` of length `chunk.len()`.
pub fn eval_expr(
    expr: &PhysicalExpr,
    chunk: &DataChunk,
    pool: &Arc<dyn MemoryPool>,
) -> Result<Vector> {
    let n = chunk.len();
    if n == 0 {
        return Ok(Vector::Flat(FlatVector::empty(expr.data_type(), pool)));
    }

    match expr {
        PhysicalExpr::ColumnRef { index, .. } => {
            let col = chunk.column(*index);
            if chunk.sel.is_identity() && col.len() == n {
                Ok(col.clone())
            } else if let Vector::Constant {
                value,
                logical_type,
                ..
            } = col
            {
                Ok(Vector::constant(value.clone(), *logical_type, n))
            } else {
                Ok(Vector::Flat(col.flatten(&chunk.sel, n, pool)?))
            }
        }
        PhysicalExpr::Literal { value, data_type } => {
            Ok(Vector::constant(value.clone(), *data_type, n))
        }
        PhysicalExpr::UnaryOp {
            op,
            expr: inner,
            return_type,
        } => {
            let v = eval_expr(inner, chunk, pool)?;
            eval_unary(*op, &v, *return_type, n, pool)
        }
        PhysicalExpr::BinaryOp {
            op,
            left,
            right,
            return_type,
        } => match op {
            BinaryOperator::And => {
                let lv = eval_expr(left, chunk, pool)?;
                let rv = eval_expr(right, chunk, pool)?;
                eval_kleene_and(&lv, &rv, n, pool)
            }
            BinaryOperator::Or => {
                let lv = eval_expr(left, chunk, pool)?;
                let rv = eval_expr(right, chunk, pool)?;
                eval_kleene_or(&lv, &rv, n, pool)
            }
            _ => {
                let lv = eval_expr(left, chunk, pool)?;
                let rv = eval_expr(right, chunk, pool)?;
                eval_binary(*op, &lv, &rv, *return_type, n, pool)
            }
        },
        PhysicalExpr::Cast {
            expr: inner,
            target_type,
            try_cast,
        } => {
            let v = eval_expr(inner, chunk, pool)?;
            if let Vector::Constant { value, .. } = &v {
                let c = cast_scalar(value, *target_type, *try_cast)?;
                return Ok(Vector::constant(c, *target_type, n));
            }
            let mut out = Vec::with_capacity(n);
            for r in 0..n {
                out.push(cast_scalar(&v.get_scalar(r), *target_type, *try_cast)?);
            }
            Vector::from_scalars(&out, *target_type, pool)
        }
        PhysicalExpr::CaseWhen {
            branches,
            else_expr,
            return_type,
        } => eval_case_when(branches, else_expr.as_deref(), *return_type, chunk, pool),
        PhysicalExpr::Coalesce { args, return_type } => {
            eval_coalesce(args, *return_type, chunk, pool)
        }
        PhysicalExpr::InList {
            expr: target,
            list,
            negated,
        } => eval_in_list(target, list, *negated, chunk, pool),
        PhysicalExpr::Like {
            expr: target,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => eval_like(
            target,
            pattern,
            *escape,
            *case_insensitive,
            *negated,
            chunk,
            pool,
        ),
        PhysicalExpr::Between {
            expr: target,
            low,
            high,
            negated,
        } => {
            let ge = PhysicalExpr::binary(
                BinaryOperator::GtEq,
                (**target).clone(),
                (**low).clone(),
                LogicalType::Boolean,
            );
            let le = PhysicalExpr::binary(
                BinaryOperator::LtEq,
                (**target).clone(),
                (**high).clone(),
                LogicalType::Boolean,
            );
            let both = PhysicalExpr::binary(BinaryOperator::And, ge, le, LogicalType::Boolean);
            let res = eval_expr(&both, chunk, pool)?;
            if *negated {
                eval_kleene_not(&res, n, pool)
            } else {
                Ok(res)
            }
        }
        PhysicalExpr::ScalarFunc {
            func,
            args,
            return_type,
        } => {
            let mut evaled = Vec::with_capacity(args.len());
            let mut all_const = true;
            for a in args {
                let v = eval_expr(a, chunk, pool)?;
                if !matches!(v, Vector::Constant { .. }) {
                    all_const = false;
                }
                evaled.push(v);
            }
            if all_const && !args.is_empty() {
                let row_args: Vec<ScalarValue> = evaled.iter().map(|v| v.get_scalar(0)).collect();
                let res = eval_scalar_function(*func, &row_args, *return_type)?;
                return Ok(Vector::constant(res, *return_type, n));
            }
            let mut out = Vec::with_capacity(n);
            let mut row_args = Vec::with_capacity(args.len());
            for r in 0..n {
                row_args.clear();
                for v in &evaled {
                    row_args.push(v.get_scalar(r));
                }
                out.push(eval_scalar_function(*func, &row_args, *return_type)?);
            }
            Vector::from_scalars(&out, *return_type, pool)
        }
    }
}

/// Evaluates a boolean filter predicate over `chunk`, returning a `SelectionVector` of
/// logical positions (`0..chunk.len()`) where the predicate evaluates to `TRUE`.
/// Uses direct AVX2/SIMD comparison kernels for common `ColumnRef <op> Literal` predicates.
pub fn eval_filter(
    predicate: &PhysicalExpr,
    chunk: &DataChunk,
    pool: &Arc<dyn MemoryPool>,
) -> Result<SelectionVector> {
    let n = chunk.len();
    if n == 0 {
        return Ok(SelectionVector::identity(0));
    }

    // Fast path: direct SIMD filter on flat identity column vs constant literal.
    if chunk.sel.is_identity() {
        if let Some(sel) = try_simd_filter_fast_path(predicate, chunk) {
            return Ok(sel);
        }
    }

    // General path: evaluate predicate vector and extract TRUE & valid indices.
    let v = eval_expr(predicate, chunk, pool)?;
    match &v {
        Vector::Constant { value, .. } => match value {
            ScalarValue::Boolean(true) => Ok(SelectionVector::identity(n)),
            _ => Ok(SelectionVector::from_vec(Vec::new())),
        },
        _ => {
            let id = SelectionVector::identity(n);
            let flat = v.flatten(&id, n, pool)?;
            let bytes = flat.as_bool_bytes();
            let mut indices = Vec::new();
            if flat.validity.is_all_valid() {
                for (i, &b) in bytes.iter().enumerate() {
                    if b != 0 {
                        indices.push(i as u16);
                    }
                }
            } else {
                for i in flat.validity.iter_valid_indices() {
                    if bytes[i] != 0 {
                        indices.push(i as u16);
                    }
                }
            }
            Ok(SelectionVector::from_vec(indices))
        }
    }
}

fn try_simd_filter_fast_path(
    predicate: &PhysicalExpr,
    chunk: &DataChunk,
) -> Option<SelectionVector> {
    let PhysicalExpr::BinaryOp {
        op, left, right, ..
    } = predicate
    else {
        return None;
    };
    let cmp_op = match op {
        BinaryOperator::Eq => CmpOp::Eq,
        BinaryOperator::NotEq => CmpOp::Ne,
        BinaryOperator::Lt => CmpOp::Lt,
        BinaryOperator::LtEq => CmpOp::Le,
        BinaryOperator::Gt => CmpOp::Gt,
        BinaryOperator::GtEq => CmpOp::Ge,
        _ => return None,
    };

    let (col_idx, lit_val, eff_op) = match (left.as_ref(), right.as_ref()) {
        (PhysicalExpr::ColumnRef { index, .. }, PhysicalExpr::Literal { value, .. }) => {
            (*index, value, cmp_op)
        }
        (PhysicalExpr::Literal { value, .. }, PhysicalExpr::ColumnRef { index, .. }) => {
            let flipped = match cmp_op {
                CmpOp::Eq => CmpOp::Eq,
                CmpOp::Ne => CmpOp::Ne,
                CmpOp::Lt => CmpOp::Gt,
                CmpOp::Le => CmpOp::Ge,
                CmpOp::Gt => CmpOp::Lt,
                CmpOp::Ge => CmpOp::Le,
            };
            (*index, value, flipped)
        }
        _ => return None,
    };

    if lit_val.is_null() {
        return Some(SelectionVector::from_vec(Vec::new()));
    }

    let Vector::Flat(flat) = chunk.column(col_idx) else {
        return None;
    };
    let n = chunk.len();
    let mut out = Vec::with_capacity(n);

    match (flat.logical_type, lit_val) {
        (LogicalType::Int64, ScalarValue::Int64(rhs))
        | (LogicalType::TimestampMicros, ScalarValue::TimestampMicros(rhs)) => {
            simd::filter_i64(&flat.as_i64_slice()[..n], *rhs, eff_op, &mut out);
        }
        (LogicalType::Int32, ScalarValue::Int32(rhs))
        | (LogicalType::Date32, ScalarValue::Date32(rhs)) => {
            simd::filter_i32(&flat.as_i32_slice()[..n], *rhs, eff_op, &mut out);
        }
        (LogicalType::Float64, ScalarValue::Float64(rhs)) => {
            simd::filter_f64(&flat.as_f64_slice()[..n], *rhs, eff_op, &mut out);
        }
        _ => return None,
    }

    if !flat.validity.is_all_valid() {
        out.retain(|&idx| flat.validity.is_valid(idx as usize));
    }
    Some(SelectionVector::from_vec(out))
}

fn eval_unary(
    op: UnaryOperator,
    input: &Vector,
    return_type: LogicalType,
    n: usize,
    pool: &Arc<dyn MemoryPool>,
) -> Result<Vector> {
    match op {
        UnaryOperator::Not => eval_kleene_not(input, n, pool),
        UnaryOperator::IsNull | UnaryOperator::IsNotNull => {
            let want_null = matches!(op, UnaryOperator::IsNull);
            match input {
                Vector::Constant { value, .. } => Ok(Vector::constant(
                    ScalarValue::Boolean(value.is_null() == want_null),
                    LogicalType::Boolean,
                    n,
                )),
                Vector::Flat(f) => {
                    if f.validity.is_all_valid() {
                        return Ok(Vector::constant(
                            ScalarValue::Boolean(!want_null),
                            LogicalType::Boolean,
                            n,
                        ));
                    }
                    let mut bytes = vec![0u8; n];
                    for (i, slot) in bytes.iter_mut().enumerate() {
                        *slot = u8::from(f.validity.is_null(i) == want_null);
                    }
                    Ok(Vector::Flat(FlatVector::from_bool_bytes(
                        &bytes,
                        ValidityMask::all_valid(n),
                        pool,
                    )?))
                }
                Vector::Dictionary { .. } => {
                    let mut bytes = vec![0u8; n];
                    for (i, slot) in bytes.iter_mut().enumerate() {
                        *slot = u8::from(input.get_scalar(i).is_null() == want_null);
                    }
                    Ok(Vector::Flat(FlatVector::from_bool_bytes(
                        &bytes,
                        ValidityMask::all_valid(n),
                        pool,
                    )?))
                }
            }
        }
        UnaryOperator::Neg => {
            if let Vector::Constant { value, .. } = input {
                let neg = eval_scalar_neg(value)?;
                return Ok(Vector::constant(neg, return_type, n));
            }
            let mut out = Vec::with_capacity(n);
            for r in 0..n {
                out.push(eval_scalar_neg(&input.get_scalar(r))?);
            }
            Vector::from_scalars(&out, return_type, pool)
        }
    }
}

pub(crate) fn eval_scalar_neg(val: &ScalarValue) -> Result<ScalarValue> {
    match val {
        ScalarValue::Null => Ok(ScalarValue::Null),
        ScalarValue::Int8(v) => v
            .checked_neg()
            .map(ScalarValue::Int8)
            .ok_or_else(|| VectraError::ArithmeticOverflow("negate i8::MIN".into())),
        ScalarValue::Int16(v) => v
            .checked_neg()
            .map(ScalarValue::Int16)
            .ok_or_else(|| VectraError::ArithmeticOverflow("negate i16::MIN".into())),
        ScalarValue::Int32(v) => v
            .checked_neg()
            .map(ScalarValue::Int32)
            .ok_or_else(|| VectraError::ArithmeticOverflow("negate i32::MIN".into())),
        ScalarValue::Int64(v) => v
            .checked_neg()
            .map(ScalarValue::Int64)
            .ok_or_else(|| VectraError::ArithmeticOverflow("negate i64::MIN".into())),
        ScalarValue::Float64(v) => Ok(ScalarValue::Float64(-*v)),
        ScalarValue::Decimal128 {
            value,
            precision,
            scale,
        } => value
            .checked_neg()
            .map(|neg| ScalarValue::Decimal128 {
                value: neg,
                precision: *precision,
                scale: *scale,
            })
            .ok_or_else(|| VectraError::ArithmeticOverflow("negate Decimal128".into())),
        other => Err(VectraError::TypeMismatch {
            expected: "numeric".into(),
            actual: other.logical_type().to_string(),
        }),
    }
}

fn eval_binary(
    op: BinaryOperator,
    left: &Vector,
    right: &Vector,
    return_type: LogicalType,
    n: usize,
    pool: &Arc<dyn MemoryPool>,
) -> Result<Vector> {
    // O(1) constant folding when both sides are ConstantVectors.
    if let (Vector::Constant { value: lv, .. }, Vector::Constant { value: rv, .. }) = (left, right)
    {
        let res = eval_scalar_binary(op, lv, rv, return_type)?;
        return Ok(Vector::constant(res, return_type, n));
    }

    // Fast SIMD vectorized Float64 Add/Sub/Mul on flat slices.
    if left.logical_type() == LogicalType::Float64
        && right.logical_type() == LogicalType::Float64
        && matches!(
            op,
            BinaryOperator::Add | BinaryOperator::Sub | BinaryOperator::Mul
        )
    {
        let id = SelectionVector::identity(n);
        let lf = left.flatten(&id, n, pool)?;
        let rf = right.flatten(&id, n, pool)?;
        let validity = lf.validity.bitand(&rf.validity);
        let mut out_buf = AlignedBufferMut::zeroed(n * 8, pool)?;
        let out_slice = out_buf.as_typed_mut_slice::<f64>();
        match op {
            BinaryOperator::Add => simd::add_f64(lf.as_f64_slice(), rf.as_f64_slice(), out_slice),
            BinaryOperator::Sub => simd::sub_f64(lf.as_f64_slice(), rf.as_f64_slice(), out_slice),
            BinaryOperator::Mul => simd::mul_f64(lf.as_f64_slice(), rf.as_f64_slice(), out_slice),
            _ => unreachable!(),
        }
        return Ok(Vector::Flat(FlatVector {
            logical_type: LogicalType::Float64,
            data: out_buf.freeze(),
            validity,
            string_heap: Vec::new(),
            len: n,
        }));
    }

    // Fast SIMD checked Int64 addition when all-valid on both sides.
    if op == BinaryOperator::Add
        && left.logical_type() == LogicalType::Int64
        && right.logical_type() == LogicalType::Int64
        && return_type == LogicalType::Int64
    {
        let id = SelectionVector::identity(n);
        let lf = left.flatten(&id, n, pool)?;
        let rf = right.flatten(&id, n, pool)?;
        if lf.validity.is_all_valid() && rf.validity.is_all_valid() {
            let mut out_buf = AlignedBufferMut::zeroed(n * 8, pool)?;
            simd::add_i64_checked(
                lf.as_i64_slice(),
                rf.as_i64_slice(),
                out_buf.as_typed_mut_slice::<i64>(),
            )?;
            return Ok(Vector::Flat(FlatVector {
                logical_type: LogicalType::Int64,
                data: out_buf.freeze(),
                validity: ValidityMask::all_valid(n),
                string_heap: Vec::new(),
                len: n,
            }));
        }
    }

    // Fast German-String `ByteView` comparison without materializing `String` allocations!
    if left.logical_type().is_varlen()
        && right.logical_type() == left.logical_type()
        && matches!(
            op,
            BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq
        )
    {
        let id = SelectionVector::identity(n);
        let lf = left.flatten(&id, n, pool)?;
        let rf = right.flatten(&id, n, pool)?;
        let validity = lf.validity.bitand(&rf.validity);
        let lv = lf.as_byte_views();
        let rv = rf.as_byte_views();
        let mut out = vec![0u8; n];
        for i in 0..n {
            if validity.is_null(i) {
                continue;
            }
            let hit = match op {
                BinaryOperator::Eq => {
                    ByteView::eq_fast(&lv[i], &lf.string_heap, &rv[i], &rf.string_heap)
                }
                BinaryOperator::NotEq => {
                    !ByteView::eq_fast(&lv[i], &lf.string_heap, &rv[i], &rf.string_heap)
                }
                BinaryOperator::Lt => {
                    ByteView::cmp_fast(&lv[i], &lf.string_heap, &rv[i], &rf.string_heap).is_lt()
                }
                BinaryOperator::LtEq => {
                    ByteView::cmp_fast(&lv[i], &lf.string_heap, &rv[i], &rf.string_heap).is_le()
                }
                BinaryOperator::Gt => {
                    ByteView::cmp_fast(&lv[i], &lf.string_heap, &rv[i], &rf.string_heap).is_gt()
                }
                BinaryOperator::GtEq => {
                    ByteView::cmp_fast(&lv[i], &lf.string_heap, &rv[i], &rf.string_heap).is_ge()
                }
                _ => unreachable!(),
            };
            out[i] = u8::from(hit);
        }
        return Ok(Vector::Flat(FlatVector::from_bool_bytes(
            &out, validity, pool,
        )?));
    }

    // General vectorized loop across all types (handles mixed decimal scales, ints, dates, distinct).
    let mut out = Vec::with_capacity(n);
    for r in 0..n {
        let lv = left.get_scalar(r);
        let rv = right.get_scalar(r);
        out.push(eval_scalar_binary(op, &lv, &rv, return_type)?);
    }
    Vector::from_scalars(&out, return_type, pool)
}

pub(crate) fn eval_scalar_binary(
    op: BinaryOperator,
    left: &ScalarValue,
    right: &ScalarValue,
    return_type: LogicalType,
) -> Result<ScalarValue> {
    // NULL-aware distinct operators first.
    match op {
        BinaryOperator::IsDistinctFrom => {
            return Ok(ScalarValue::Boolean(left != right));
        }
        BinaryOperator::IsNotDistinctFrom => {
            return Ok(ScalarValue::Boolean(left == right));
        }
        BinaryOperator::And => {
            return Ok(match (left, right) {
                (ScalarValue::Boolean(false), _) | (_, ScalarValue::Boolean(false)) => {
                    ScalarValue::Boolean(false)
                }
                (ScalarValue::Boolean(true), ScalarValue::Boolean(true)) => {
                    ScalarValue::Boolean(true)
                }
                _ => ScalarValue::Null,
            });
        }
        BinaryOperator::Or => {
            return Ok(match (left, right) {
                (ScalarValue::Boolean(true), _) | (_, ScalarValue::Boolean(true)) => {
                    ScalarValue::Boolean(true)
                }
                (ScalarValue::Boolean(false), ScalarValue::Boolean(false)) => {
                    ScalarValue::Boolean(false)
                }
                _ => ScalarValue::Null,
            });
        }
        _ => {}
    }

    // Strict operators: NULL propagates.
    if left.is_null() || right.is_null() {
        return Ok(ScalarValue::Null);
    }

    if op == BinaryOperator::Concat {
        return Ok(ScalarValue::Utf8(format!(
            "{}{}",
            left.to_display_string(),
            right.to_display_string()
        )));
    }

    if op.is_comparison() {
        return eval_scalar_cmp(op, left, right);
    }

    // Arithmetic operators
    match (left, right) {
        (ScalarValue::Int8(a), ScalarValue::Int8(b)) => eval_int_arith(
            op,
            *a as i128,
            *b as i128,
            i8::MIN as i128,
            i8::MAX as i128,
            |v| ScalarValue::Int8(v as i8),
        ),
        (ScalarValue::Int16(a), ScalarValue::Int16(b)) => eval_int_arith(
            op,
            *a as i128,
            *b as i128,
            i16::MIN as i128,
            i16::MAX as i128,
            |v| ScalarValue::Int16(v as i16),
        ),
        (ScalarValue::Int32(a), ScalarValue::Int32(b)) => eval_int_arith(
            op,
            *a as i128,
            *b as i128,
            i32::MIN as i128,
            i32::MAX as i128,
            |v| ScalarValue::Int32(v as i32),
        ),
        (ScalarValue::Int64(a), ScalarValue::Int64(b)) => eval_i64_arith(op, *a, *b),
        (ScalarValue::Float64(a), ScalarValue::Float64(b)) => match op {
            BinaryOperator::Add => Ok(ScalarValue::Float64(*a + *b)),
            BinaryOperator::Sub => Ok(ScalarValue::Float64(*a - *b)),
            BinaryOperator::Mul => Ok(ScalarValue::Float64(*a * *b)),
            BinaryOperator::Div => {
                if *b == 0.0 {
                    Err(VectraError::DivisionByZero)
                } else {
                    Ok(ScalarValue::Float64(*a / *b))
                }
            }
            BinaryOperator::Mod => {
                if *b == 0.0 {
                    Err(VectraError::DivisionByZero)
                } else {
                    Ok(ScalarValue::Float64(*a % *b))
                }
            }
            _ => unreachable!(),
        },
        (
            ScalarValue::Decimal128 {
                value: va,
                scale: sa,
                ..
            },
            ScalarValue::Decimal128 {
                value: vb,
                scale: sb,
                ..
            },
        ) => {
            let (out_p, out_s) = match return_type {
                LogicalType::Decimal128 { precision, scale } => (precision, scale),
                _ => (MAX_DECIMAL_PRECISION, (*sa).max(*sb)),
            };
            let v = match op {
                BinaryOperator::Add => decimal_add(*va, *sa, *vb, *sb, out_p, out_s)?,
                BinaryOperator::Sub => decimal_sub(*va, *sa, *vb, *sb, out_p, out_s)?,
                BinaryOperator::Mul => decimal_mul(*va, *sa, *vb, *sb, out_p, out_s)?,
                BinaryOperator::Div => decimal_div(*va, *sa, *vb, *sb, out_p, out_s)?,
                BinaryOperator::Mod => {
                    if *vb == 0 {
                        return Err(VectraError::DivisionByZero);
                    }
                    let common_s = (*sa).max(*sb);
                    let ra = decimal_rescale(*va, *sa, common_s, MAX_DECIMAL_PRECISION)?;
                    let rb = decimal_rescale(*vb, *sb, common_s, MAX_DECIMAL_PRECISION)?;
                    let rem = ra % rb;
                    decimal_rescale(rem, common_s, out_s, out_p)?
                }
                _ => unreachable!(),
            };
            Ok(ScalarValue::Decimal128 {
                value: v,
                precision: out_p,
                scale: out_s,
            })
        }
        (a, b) => Err(VectraError::TypeMismatch {
            expected: format!("compatible operands for {op:?}"),
            actual: format!("{}, {}", a.logical_type(), b.logical_type()),
        }),
    }
}

fn eval_int_arith<F>(
    op: BinaryOperator,
    a: i128,
    b: i128,
    min: i128,
    max: i128,
    wrap: F,
) -> Result<ScalarValue>
where
    F: FnOnce(i128) -> ScalarValue,
{
    let res = match op {
        BinaryOperator::Add => a.checked_add(b),
        BinaryOperator::Sub => a.checked_sub(b),
        BinaryOperator::Mul => a.checked_mul(b),
        BinaryOperator::Div => {
            if b == 0 {
                return Err(VectraError::DivisionByZero);
            }
            a.checked_div(b)
        }
        BinaryOperator::Mod => {
            if b == 0 {
                return Err(VectraError::DivisionByZero);
            }
            a.checked_rem(b)
        }
        _ => unreachable!(),
    };
    let v = res.ok_or_else(|| VectraError::ArithmeticOverflow(format!("{a} {op:?} {b}")))?;
    if v < min || v > max {
        return Err(VectraError::ArithmeticOverflow(format!(
            "{a} {op:?} {b} out of range [{min}, {max}]"
        )));
    }
    Ok(wrap(v))
}

fn eval_i64_arith(op: BinaryOperator, a: i64, b: i64) -> Result<ScalarValue> {
    let v = match op {
        BinaryOperator::Add => a.checked_add(b),
        BinaryOperator::Sub => a.checked_sub(b),
        BinaryOperator::Mul => a.checked_mul(b),
        BinaryOperator::Div => {
            if b == 0 {
                return Err(VectraError::DivisionByZero);
            }
            a.checked_div(b)
        }
        BinaryOperator::Mod => {
            if b == 0 {
                return Err(VectraError::DivisionByZero);
            }
            a.checked_rem(b)
        }
        _ => unreachable!(),
    };
    v.map(ScalarValue::Int64)
        .ok_or_else(|| VectraError::ArithmeticOverflow(format!("{a} {op:?} {b} overflows i64")))
}

fn eval_scalar_cmp(
    op: BinaryOperator,
    left: &ScalarValue,
    right: &ScalarValue,
) -> Result<ScalarValue> {
    // IEEE-754 semantics for Float64 comparisons (matches AVX2 SIMD kernels bit-for-bit).
    if let (ScalarValue::Float64(a), ScalarValue::Float64(b)) = (left, right) {
        let hit = match op {
            BinaryOperator::Eq => a == b,
            BinaryOperator::NotEq => a != b,
            BinaryOperator::Lt => a < b,
            BinaryOperator::LtEq => a <= b,
            BinaryOperator::Gt => a > b,
            BinaryOperator::GtEq => a >= b,
            _ => unreachable!(),
        };
        return Ok(ScalarValue::Boolean(hit));
    }
    let ord = left.total_cmp(right);
    let hit = match op {
        BinaryOperator::Eq => ord.is_eq(),
        BinaryOperator::NotEq => ord.is_ne(),
        BinaryOperator::Lt => ord.is_lt(),
        BinaryOperator::LtEq => ord.is_le(),
        BinaryOperator::Gt => ord.is_gt(),
        BinaryOperator::GtEq => ord.is_ge(),
        _ => unreachable!(),
    };
    Ok(ScalarValue::Boolean(hit))
}

/// Short-circuit `CASE WHEN` using `SelectionVector` narrowing so unreached `THEN`/`ELSE`
/// branches are NEVER evaluated on already-matched rows.
fn eval_case_when(
    branches: &[(PhysicalExpr, PhysicalExpr)],
    else_expr: Option<&PhysicalExpr>,
    return_type: LogicalType,
    chunk: &DataChunk,
    pool: &Arc<dyn MemoryPool>,
) -> Result<Vector> {
    let n = chunk.len();
    let mut results = vec![ScalarValue::Null; n];
    let mut remaining: Vec<u16> = (0..n as u16).collect();

    for (cond, then_expr) in branches {
        if remaining.is_empty() {
            break;
        }
        let sub_chunk = chunk.filter(&SelectionVector::from_slice(&remaining));
        let cond_vec = eval_expr(cond, &sub_chunk, pool)?;

        let mut matched_pos = Vec::new();
        let mut next_remaining = Vec::new();
        let mut matched_orig_rows = Vec::new();

        for (sub_i, &orig_row) in remaining.iter().enumerate() {
            if matches!(cond_vec.get_scalar(sub_i), ScalarValue::Boolean(true)) {
                matched_pos.push(sub_i as u16);
                matched_orig_rows.push(orig_row as usize);
            } else {
                next_remaining.push(orig_row);
            }
        }

        if !matched_pos.is_empty() {
            let then_chunk = sub_chunk.filter(&SelectionVector::from_vec(matched_pos));
            let then_vec = eval_expr(then_expr, &then_chunk, pool)?;
            for (k, orig_row) in matched_orig_rows.into_iter().enumerate() {
                results[orig_row] = then_vec.get_scalar(k);
            }
        }
        remaining = next_remaining;
    }

    if !remaining.is_empty() {
        if let Some(else_e) = else_expr {
            let else_chunk = chunk.filter(&SelectionVector::from_slice(&remaining));
            let else_vec = eval_expr(else_e, &else_chunk, pool)?;
            for (k, &orig_row) in remaining.iter().enumerate() {
                results[orig_row as usize] = else_vec.get_scalar(k);
            }
        }
    }

    Vector::from_scalars(&results, return_type, pool)
}

/// Short-circuit `COALESCE` using `SelectionVector` narrowing.
fn eval_coalesce(
    args: &[PhysicalExpr],
    return_type: LogicalType,
    chunk: &DataChunk,
    pool: &Arc<dyn MemoryPool>,
) -> Result<Vector> {
    let n = chunk.len();
    let mut results = vec![ScalarValue::Null; n];
    let mut remaining: Vec<u16> = (0..n as u16).collect();

    for arg in args {
        if remaining.is_empty() {
            break;
        }
        let sub_chunk = chunk.filter(&SelectionVector::from_slice(&remaining));
        let vec = eval_expr(arg, &sub_chunk, pool)?;
        let mut next_remaining = Vec::new();
        for (sub_i, &orig_row) in remaining.iter().enumerate() {
            let v = vec.get_scalar(sub_i);
            if v.is_null() {
                next_remaining.push(orig_row);
            } else {
                results[orig_row as usize] = v;
            }
        }
        remaining = next_remaining;
    }

    Vector::from_scalars(&results, return_type, pool)
}

/// Vectorized `IN (...)` / `NOT IN (...)` with O(1) HashSet fast path when `list.len() >= 8`
/// and all items are constant literals, preserving exact 3-valued SQL NULL semantics.
fn eval_in_list(
    target: &PhysicalExpr,
    list: &[PhysicalExpr],
    negated: bool,
    chunk: &DataChunk,
    pool: &Arc<dyn MemoryPool>,
) -> Result<Vector> {
    let n = chunk.len();
    if list.is_empty() {
        return Ok(Vector::constant(
            ScalarValue::Boolean(negated),
            LogicalType::Boolean,
            n,
        ));
    }

    let lhs = eval_expr(target, chunk, pool)?;

    // Hash-set fast path for >= 8 constant literals (non-Float64 so NaN IEEE inequality is preserved).
    let all_literals = list
        .iter()
        .all(|e| matches!(e, PhysicalExpr::Literal { .. }));
    if all_literals && list.len() >= 8 && lhs.logical_type() != LogicalType::Float64 {
        let mut set = HashSet::with_capacity(list.len());
        let mut has_null = false;
        for item in list {
            if let PhysicalExpr::Literal { value, .. } = item {
                if value.is_null() {
                    has_null = true;
                } else {
                    set.insert(value.clone());
                }
            }
        }
        let mut bytes = vec![0u8; n];
        let mut mask = ValidityMaskBuilder::with_capacity(n);
        for (r, slot) in bytes.iter_mut().enumerate() {
            let v = lhs.get_scalar(r);
            if v.is_null() {
                mask.push(false);
            } else if set.contains(&v) {
                mask.push(true);
                *slot = u8::from(!negated);
            } else if has_null {
                mask.push(false);
            } else {
                mask.push(true);
                *slot = u8::from(negated);
            }
        }
        return Ok(Vector::Flat(FlatVector::from_bool_bytes(
            &bytes,
            mask.finish(),
            pool,
        )?));
    }

    // General path supporting arbitrary dynamic expressions in list & exact Float64 NaN rules.
    let mut item_vecs = Vec::with_capacity(list.len());
    for item in list {
        item_vecs.push(eval_expr(item, chunk, pool)?);
    }

    let mut bytes = vec![0u8; n];
    let mut mask = ValidityMaskBuilder::with_capacity(n);
    for (r, slot) in bytes.iter_mut().enumerate() {
        let v = lhs.get_scalar(r);
        if v.is_null() {
            mask.push(false);
            continue;
        }
        let mut found_match = false;
        let mut saw_null = false;
        for iv in &item_vecs {
            let candidate = iv.get_scalar(r);
            if candidate.is_null() {
                saw_null = true;
            } else if matches!(
                eval_scalar_cmp(BinaryOperator::Eq, &v, &candidate)?,
                ScalarValue::Boolean(true)
            ) {
                found_match = true;
                break;
            }
        }
        if found_match {
            mask.push(true);
            *slot = u8::from(!negated);
        } else if saw_null {
            mask.push(false);
        } else {
            mask.push(true);
            *slot = u8::from(negated);
        }
    }

    Ok(Vector::Flat(FlatVector::from_bool_bytes(
        &bytes,
        mask.finish(),
        pool,
    )?))
}

fn eval_like(
    target: &PhysicalExpr,
    pattern: &PhysicalExpr,
    escape: Option<char>,
    case_insensitive: bool,
    negated: bool,
    chunk: &DataChunk,
    pool: &Arc<dyn MemoryPool>,
) -> Result<Vector> {
    let n = chunk.len();
    let tv = eval_expr(target, chunk, pool)?;

    // Fast path: compile constant pattern once per chunk!
    if let PhysicalExpr::Literal { value, .. } = pattern {
        if value.is_null() {
            return Ok(Vector::constant(ScalarValue::Null, LogicalType::Boolean, n));
        }
        let ScalarValue::Utf8(pat_str) = value else {
            return Err(VectraError::TypeMismatch {
                expected: "Utf8".into(),
                actual: value.logical_type().to_string(),
            });
        };
        let compiled = CompiledLike::compile(pat_str, escape, case_insensitive);
        let id = SelectionVector::identity(n);
        let flat = tv.flatten(&id, n, pool)?;
        let views = flat.as_byte_views();
        let mut bytes = vec![0u8; n];
        for (i, slot) in bytes.iter_mut().enumerate() {
            if flat.validity.is_null(i) {
                continue;
            }
            let s = std::str::from_utf8(views[i].as_bytes(&flat.string_heap))
                .map_err(|e| VectraError::ExecutionError(format!("invalid UTF-8 in LIKE: {e}")))?;
            let matched = compiled.is_match(s, case_insensitive);
            *slot = u8::from(if negated { !matched } else { matched });
        }
        return Ok(Vector::Flat(FlatVector::from_bool_bytes(
            &bytes,
            flat.validity,
            pool,
        )?));
    }

    // Dynamic pattern path.
    let pv = eval_expr(pattern, chunk, pool)?;
    let mut bytes = vec![0u8; n];
    let mut mask = ValidityMaskBuilder::with_capacity(n);
    for (r, slot) in bytes.iter_mut().enumerate() {
        let a = tv.get_scalar(r);
        let b = pv.get_scalar(r);
        match (a, b) {
            (ScalarValue::Null, _) | (_, ScalarValue::Null) => mask.push(false),
            (ScalarValue::Utf8(ts), ScalarValue::Utf8(ps)) => {
                let m = like_match(&ts, &ps, escape, case_insensitive);
                mask.push(true);
                *slot = u8::from(if negated { !m } else { m });
            }
            (x, y) => {
                return Err(VectraError::TypeMismatch {
                    expected: "Utf8".into(),
                    actual: format!("{}, {}", x.logical_type(), y.logical_type()),
                });
            }
        }
    }
    Ok(Vector::Flat(FlatVector::from_bool_bytes(
        &bytes,
        mask.finish(),
        pool,
    )?))
}
