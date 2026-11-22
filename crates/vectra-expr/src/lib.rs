pub mod cast;
pub mod eval;
pub mod expr;
pub mod functions;
pub mod hash;
pub mod kleene;
pub mod like;
pub mod reference;
pub mod simd;

pub use cast::cast_scalar;
pub use eval::{eval_expr, eval_filter};
pub use expr::{BinaryOperator, DatePart, PhysicalExpr, ScalarFunction, UnaryOperator};
pub use functions::eval_scalar_function;
pub use hash::{hash_chunk, hash_combine, hash_vector};
pub use kleene::{
    eval_kleene_and, eval_kleene_not, eval_kleene_or, kleene_and_word, kleene_not_word,
    kleene_or_word,
};
pub use like::{like_match, CompiledLike};
pub use reference::eval_expr_scalar_reference;
pub use simd::{has_avx2, has_neon, CmpOp};
