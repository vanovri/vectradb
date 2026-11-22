use serde::{Deserialize, Serialize};
use vectra_types::VectraError;

/// Byte-offset span inside a SQL source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    #[inline]
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    #[inline]
    pub fn merge(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// Renders a multi-line caret diagnostic with `line:col`, source snippet, `^^^^` underline, and hint.
pub fn render_diagnostic(sql: &str, span: Span, message: &str, hint: Option<&str>) -> String {
    let start = (span.start as usize).min(sql.len());
    let end = (span.end as usize).max(start).min(sql.len());

    let mut line_no = 1usize;
    let mut line_start = 0usize;
    for (idx, ch) in sql.char_indices() {
        if idx >= start {
            break;
        }
        if ch == '\n' {
            line_no += 1;
            line_start = idx + 1;
        }
    }
    let line_end = sql[line_start..]
        .find('\n')
        .map(|off| line_start + off)
        .unwrap_or(sql.len());
    let line_text = sql[line_start..line_end].trim_end_matches('\r');
    let col = sql[line_start..start].chars().count() + 1;
    let underline_len = if end > start {
        let clamped_end = end.min(line_end);
        sql[start..clamped_end].chars().count().max(1)
    } else {
        1
    };

    let gutter = format!("{line_no}");
    let pad = " ".repeat(gutter.len());
    let caret_pad = " ".repeat(col.saturating_sub(1));
    let carets = "^".repeat(underline_len);

    let mut out = format!(
        "error at {line_no}:{col}: {message}\n{pad} |\n{gutter} | {line_text}\n{pad} | {caret_pad}{carets}"
    );
    if let Some(h) = hint {
        out.push_str(&format!("\n{pad} = hint: {h}"));
    }
    out
}

pub fn parse_error(
    sql: &str,
    span: Span,
    message: impl Into<String>,
    hint: Option<&str>,
) -> VectraError {
    let msg = message.into();
    let rendered = render_diagnostic(sql, span, &msg, hint);
    VectraError::ParseError {
        message: msg,
        span: Some((span.start, span.end)),
        rendered,
    }
}
