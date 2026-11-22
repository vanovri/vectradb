use memchr::memmem;

#[derive(Debug, Clone)]
pub enum PatToken {
    WildcardMany, // %
    WildcardOne,  // _
    Literal(char),
}

/// Precompiled SQL `LIKE` / `ILIKE` matcher avoiding exponential backtracking.
#[derive(Debug)]
pub enum CompiledLike {
    Exact(String),
    Prefix(String),
    Suffix(String),
    Contains(Box<memmem::Finder<'static>>, String),
    General(Vec<PatToken>),
}

impl CompiledLike {
    pub fn compile(pattern: &str, escape: Option<char>, case_insensitive: bool) -> Self {
        let pat_str = if case_insensitive {
            pattern.to_lowercase()
        } else {
            pattern.to_string()
        };

        let mut tokens = Vec::new();
        let mut chars = pat_str.chars().peekable();
        while let Some(c) = chars.next() {
            if Some(c) == escape {
                if let Some(escaped) = chars.next() {
                    tokens.push(PatToken::Literal(escaped));
                } else {
                    tokens.push(PatToken::Literal(c));
                }
            } else if c == '%' {
                // Coalesce consecutive `%` wildcards.
                if !matches!(tokens.last(), Some(PatToken::WildcardMany)) {
                    tokens.push(PatToken::WildcardMany);
                }
            } else if c == '_' {
                tokens.push(PatToken::WildcardOne);
            } else {
                tokens.push(PatToken::Literal(c));
            }
        }

        // Fast-path classification when no `_` exists.
        let has_one = tokens.iter().any(|t| matches!(t, PatToken::WildcardOne));
        if !has_one {
            let many_count = tokens
                .iter()
                .filter(|t| matches!(t, PatToken::WildcardMany))
                .count();
            match many_count {
                0 => {
                    let s: String = tokens
                        .into_iter()
                        .map(|t| match t {
                            PatToken::Literal(c) => c,
                            _ => unreachable!(),
                        })
                        .collect();
                    return Self::Exact(s);
                }
                1 => {
                    if matches!(tokens.first(), Some(PatToken::WildcardMany)) {
                        let s: String = tokens[1..]
                            .iter()
                            .map(|t| match t {
                                PatToken::Literal(c) => *c,
                                _ => unreachable!(),
                            })
                            .collect();
                        return Self::Suffix(s);
                    }
                    if matches!(tokens.last(), Some(PatToken::WildcardMany)) {
                        let s: String = tokens[..tokens.len() - 1]
                            .iter()
                            .map(|t| match t {
                                PatToken::Literal(c) => *c,
                                _ => unreachable!(),
                            })
                            .collect();
                        return Self::Prefix(s);
                    }
                }
                2 if matches!(tokens.first(), Some(PatToken::WildcardMany))
                    && matches!(tokens.last(), Some(PatToken::WildcardMany)) =>
                {
                    let s: String = tokens[1..tokens.len() - 1]
                        .iter()
                        .map(|t| match t {
                            PatToken::Literal(c) => *c,
                            _ => unreachable!(),
                        })
                        .collect();
                    let finder = Box::new(memmem::Finder::new(s.as_bytes()).into_owned());
                    return Self::Contains(finder, s);
                }
                _ => {}
            }
        }

        Self::General(tokens)
    }

    pub fn is_match(&self, text: &str, case_insensitive: bool) -> bool {
        let owned;
        let hay: &str = if case_insensitive {
            owned = text.to_lowercase();
            &owned
        } else {
            text
        };
        match self {
            Self::Exact(lit) => hay == lit,
            Self::Prefix(pre) => hay.starts_with(pre.as_str()),
            Self::Suffix(suf) => hay.ends_with(suf.as_str()),
            Self::Contains(finder, needle) => {
                if needle.is_empty() {
                    true
                } else {
                    finder.find(hay.as_bytes()).is_some()
                }
            }
            Self::General(tokens) => match_tokens_linear(hay, tokens),
        }
    }
}

/// Linear-space, guaranteed O(|text| * |pattern|) iterative two-pointer LIKE matcher.
/// Never backtracks exponentially on pathological patterns like `%a%a%a%a%b`.
fn match_tokens_linear(text: &str, pat: &[PatToken]) -> bool {
    let t: Vec<char> = text.chars().collect();
    let mut ti = 0usize;
    let mut pi = 0usize;
    let mut star_pi: Option<usize> = None;
    let mut star_ti = 0usize;

    while ti < t.len() {
        if pi < pat.len() {
            match pat[pi] {
                PatToken::WildcardMany => {
                    star_pi = Some(pi);
                    star_ti = ti;
                    pi += 1;
                    continue;
                }
                PatToken::WildcardOne => {
                    ti += 1;
                    pi += 1;
                    continue;
                }
                PatToken::Literal(c) if c == t[ti] => {
                    ti += 1;
                    pi += 1;
                    continue;
                }
                _ => {}
            }
        }
        if let Some(sp) = star_pi {
            star_ti += 1;
            ti = star_ti;
            pi = sp + 1;
        } else {
            return false;
        }
    }

    while pi < pat.len() && matches!(pat[pi], PatToken::WildcardMany) {
        pi += 1;
    }
    pi == pat.len()
}

pub fn like_match(text: &str, pattern: &str, escape: Option<char>, case_insensitive: bool) -> bool {
    CompiledLike::compile(pattern, escape, case_insensitive).is_match(text, case_insensitive)
}
