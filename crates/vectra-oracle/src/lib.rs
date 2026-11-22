#![deny(unsafe_code)]

pub mod md5;
pub mod shrinker;
pub mod slt;
pub mod sqlite_oracle;
pub mod sqlsmith;

pub use md5::md5_hex;
pub use shrinker::{shrink_failing_sql, write_failure_reproducer};
pub use slt::{
    format_slt_value, parse_slt, run_slt_directory, run_slt_file, run_slt_str, SltExpected,
    SltRecord, SltSortMode, SltStats,
};
pub use sqlite_oracle::{compare_oracle_results, OracleValue, SqliteOracle};
pub use sqlsmith::{
    build_sqlsmith_setup_sql, generate_sqlsmith_query, run_sqlsmith_campaign, GeneratedQuery,
    SqlsmithFailure, SqlsmithReport,
};
