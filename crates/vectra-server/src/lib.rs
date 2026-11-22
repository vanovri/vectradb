#![deny(unsafe_code)]

pub mod cli;
pub mod pgwire;

pub use cli::{format_query_result, format_unicode_box_table, CliState, OutputMode};
pub use pgwire::{PgServer, PgWireClient, SimpleQueryOutput};
