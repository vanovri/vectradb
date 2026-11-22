//! Simple, independent, row-by-row reference evaluator used as the ground-truth
//! differential oracle for verifying the vectorized Expression VM and SIMD kernels.

use crate::cast::cast_scalar;
use crate::eval::{eval_scalar_binary, eval_scalar_neg};
use crate::expr::{BinaryOperator, PhysicalExpr, UnaryOperator};
use crate::functions::eval_scalar_function;
use crate::like::like_match;
use vectra_types::{LogicalType, Result, ScalarValue, VectraError};

/// Evaluates `expr` on a single materialized row `row: &[ScalarValue]`.
pub fn eval_expr_scalar_reference(expr: &PhysicalExpr, row: &[ScalarValue]) -> Result<ScalarValue> {
    match expr {
        PhysicalExpr::ColumnRef { index, .. } => Ok(row[*index].clone()),
        PhysicalExpr::Literal { value, .. } => Ok(value.clone()),
        PhysicalExpr::UnaryOp { op, expr, .. } => {
            let v = eval_expr_scalar_reference(expr, row)?;
            match op {
                UnaryOperator::IsNull => Ok(ScalarValue::Boolean(v.is_null())),
                UnaryOperator::IsNotNull => Ok(ScalarValue::Boolean(!v.is_null())),
                UnaryOperator::Not => Ok(match v {
                    ScalarValue::Null => ScalarValue::Null,
                    ScalarValue::Boolean(b) => ScalarValue::Boolean(!b),
                    other => {
                        return Err(VectraError::TypeMismatch {
                            expected: "Boolean".into(),
                            actual: other.logical_type().to_string(),
                        })
                    }
                }),
                UnaryOperator::Neg => eval_scalar_neg(&v),
            }
        }
        PhysicalExpr::BinaryOp {
            op,
            left,
            right,
            return_type,
        } => {
            let lv = eval_expr_scalar_reference(left, row)?;
            let rv = eval_expr_scalar_reference(right, row)?;
            eval_scalar_binary(*op, &lv, &rv, *return_type)
        }
        PhysicalExpr::Cast {
            expr,
            target_type,
            try_cast,
        } => {
            let v = eval_expr_scalar_reference(expr, row)?;
            cast_scalar(&v, *target_type, *try_cast)
        }
        PhysicalExpr::CaseWhen {
            branches,
            else_expr,
            ..
        } => {
            for (cond, then_e) in branches {
                let cv = eval_expr_scalar_reference(cond, row)?;
                if matches!(cv, ScalarValue::Boolean(true)) {
                    return eval_expr_scalar_reference(then_e, row);
                }
            }
            match else_expr {
                Some(e) => eval_expr_scalar_reference(e, row),
                None => Ok(ScalarValue::Null),
            }
        }
        PhysicalExpr::Coalesce { args, .. } => {
            for a in args {
                let v = eval_expr_scalar_reference(a, row)?;
                if !v.is_null() {
                    return Ok(v);
                }
            }
            Ok(ScalarValue::Null)
        }
        PhysicalExpr::InList {
            expr,
            list,
            negated,
        } => {
            if list.is_empty() {
                return Ok(ScalarValue::Boolean(*negated));
            }
            let lv = eval_expr_scalar_reference(expr, row)?;
            if lv.is_null() {
                return Ok(ScalarValue::Null);
            }
            let mut saw_null = false;
            for item in list {
                let rv = eval_expr_scalar_reference(item, row)?;
                if rv.is_null() {
                    saw_null = true;
                } else if matches!(
                    eval_scalar_binary(BinaryOperator::Eq, &lv, &rv, LogicalType::Boolean)?,
                    ScalarValue::Boolean(true)
                ) {
                    return Ok(ScalarValue::Boolean(!*negated));
                }
            }
            if saw_null {
                Ok(ScalarValue::Null)
            } else {
                Ok(ScalarValue::Boolean(*negated))
            }
        }
        PhysicalExpr::Like {
            expr,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => {
            let tv = eval_expr_scalar_reference(expr, row)?;
            let pv = eval_expr_scalar_reference(pattern, row)?;
            match (tv, pv) {
                (ScalarValue::Null, _) | (_, ScalarValue::Null) => Ok(ScalarValue::Null),
                (ScalarValue::Utf8(ts), ScalarValue::Utf8(ps)) => {
                    let m = like_match(&ts, &ps, *escape, *case_insensitive);
                    Ok(ScalarValue::Boolean(if *negated { !m } else { m }))
                }
                (a, b) => Err(VectraError::TypeMismatch {
                    expected: "Utf8".into(),
                    actual: format!("{}, {}", a.logical_type(), b.logical_type()),
                }),
            }
        }
        PhysicalExpr::Between {
            expr,
            low,
            high,
            negated,
        } => {
            let v = eval_expr_scalar_reference(expr, row)?;
            let l = eval_expr_scalar_reference(low, row)?;
            let h = eval_expr_scalar_reference(high, row)?;
            let ge = eval_scalar_binary(BinaryOperator::GtEq, &v, &l, LogicalType::Boolean)?;
            let le = eval_scalar_binary(BinaryOperator::LtEq, &v, &h, LogicalType::Boolean)?;
            let both = eval_scalar_binary(BinaryOperator::And, &ge, &le, LogicalType::Boolean)?;
            if *negated {
                Ok(match both {
                    ScalarValue::Null => ScalarValue::Null,
                    ScalarValue::Boolean(b) => ScalarValue::Boolean(!b),
                    _ => unreachable!(),
                })
            } else {
                Ok(both)
            }
        }
        PhysicalExpr::ScalarFunc {
            func,
            args,
            return_type,
        } => {
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(eval_expr_scalar_reference(a, row)?);
            }
            eval_scalar_function(*func, &vals, *return_type)
        }
    }
}
