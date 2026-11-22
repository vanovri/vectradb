#![deny(unsafe_code)]

pub mod dbgen;
pub mod queries;
pub mod schema;
#[cfg(feature = "oracle")]
pub mod validation;

pub use dbgen::{generate_tpch_streaming, load_tpch_into_database, TpchLoadSummary};
pub use queries::{tpch_query, translate_tpch_to_sqlite, TpchQuery, TPCH_QUERIES};
pub use schema::all_tpch_schemas;
#[cfg(feature = "oracle")]
pub use validation::{validate_all_22_against_sqlite, QueryValidationResult};
