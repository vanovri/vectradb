#![deny(unsafe_code)]

pub mod ast;
pub mod binder;
pub mod coercion;
pub mod diagnostics;
pub mod lexer;
pub mod logical_plan;
pub mod parser;
pub mod pretty;
pub mod unnest;

pub use ast::*;
pub use binder::Binder;
pub use coercion::{arithmetic_result_type, coerce_expr, common_supertype};
pub use diagnostics::{parse_error, render_diagnostic, Span};
pub use lexer::{tokenize, Keyword, Token, TokenKind};
pub use logical_plan::{
    AggregateExpr, AggregateFunc, BoundCol, ColId, ColIdGen, JoinType, LogicalExpr, LogicalPlan,
    LogicalSortExpr, LogicalStatement, TableFunctionKind, WindowExpr, WindowFunc,
};
pub use parser::{parse_single_statement, parse_sql};
pub use pretty::{pretty_print_expr, pretty_print_query, pretty_print_statement};
pub use unnest::{clone_plan_fresh, unnest_plan};
