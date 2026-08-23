//! Statement splitter: aware of quoting and comments, so a semicolon inside
//! a string literal, a comment, or a dollar-quoted function body does not
//! split the statement. Block comments nest (`/* outer /* inner */ still
//! comment */` is one comment, matching PostgreSQL), tracked with a depth
//! counter.

#[derive(PartialEq)]
enum State {
    Top,
    SingleQuote,
    DoubleQuote,
    LineComment,
    BlockComment,
    /// Inside `$tag$ ... $tag$`. Holds the tag (without the `$` delimiters).
    DollarQuote,
}

/// Split `sql` into individual statements, dropping empty ones (trailing
/// semicolons, blank input, comment-only trailers).
pub fn split(sql: &str) -> Vec<String> {
    let chars: Vec<char> = sql.chars().collect();
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut state = State::Top;
    let mut dollar_tag = String::new();
    let mut comment_depth: u32 = 0;
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        match state {
            State::Top => {
                if c == '\'' {
                    state = State::SingleQuote;
                    current.push(c);
                    i += 1;
                } else if c == '"' {
                    state = State::DoubleQuote;
                    current.push(c);
                    i += 1;
                } else if c == '-' && chars.get(i + 1) == Some(&'-') {
                    state = State::LineComment;
                    current.push(c);
                    current.push('-');
                    i += 2;
                } else if c == '/' && chars.get(i + 1) == Some(&'*') {
                    state = State::BlockComment;
                    comment_depth = 1;
                    current.push(c);
                    current.push('*');
                    i += 2;
                } else if c == '$' {
                    if let Some((tag, len)) = match_dollar_tag(&chars[i..]) {
                        dollar_tag = tag;
                        state = State::DollarQuote;
                        current.push_str(&chars[i..i + len].iter().collect::<String>());
                        i += len;
                    } else {
                        current.push(c);
                        i += 1;
                    }
                } else if c == ';' {
                    let trimmed = current.trim();
                    if !trimmed.is_empty() {
                        statements.push(trimmed.to_string());
                    }
                    current.clear();
                    i += 1;
                } else {
                    current.push(c);
                    i += 1;
                }
            }
            State::SingleQuote => {
                current.push(c);
                if c == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        current.push('\'');
                        i += 2;
                        continue;
                    }
                    state = State::Top;
                }
                i += 1;
            }
            State::DoubleQuote => {
                current.push(c);
                if c == '"' {
                    if chars.get(i + 1) == Some(&'"') {
                        current.push('"');
                        i += 2;
                        continue;
                    }
                    state = State::Top;
                }
                i += 1;
            }
            State::LineComment => {
                current.push(c);
                if c == '\n' {
                    state = State::Top;
                }
                i += 1;
            }
            State::BlockComment => {
                if c == '/' && chars.get(i + 1) == Some(&'*') {
                    comment_depth += 1;
                    current.push(c);
                    current.push('*');
                    i += 2;
                    continue;
                }
                if c == '*' && chars.get(i + 1) == Some(&'/') {
                    comment_depth -= 1;
                    current.push('*');
                    current.push('/');
                    i += 2;
                    if comment_depth == 0 {
                        state = State::Top;
                    }
                    continue;
                }
                current.push(c);
                i += 1;
            }
            State::DollarQuote => {
                if c == '$' {
                    let closing = format!("${dollar_tag}$");
                    let closing_chars: Vec<char> = closing.chars().collect();
                    if chars[i..].starts_with(&closing_chars[..]) {
                        current.push_str(&closing);
                        i += closing_chars.len();
                        state = State::Top;
                        continue;
                    }
                }
                current.push(c);
                i += 1;
            }
        }
    }

    let trimmed = current.trim();
    if !trimmed.is_empty() {
        statements.push(trimmed.to_string());
    }

    statements
}

/// If `chars` starts with a dollar-quote opener (`$$` or `$tag$`), return the
/// tag (empty for `$$`) and the length in chars of the opener.
fn match_dollar_tag(chars: &[char]) -> Option<(String, usize)> {
    if chars.first() != Some(&'$') {
        return None;
    }
    let mut end = 1;
    while let Some(&c) = chars.get(end) {
        if c == '$' {
            return Some((chars[1..end].iter().collect(), end + 1));
        }
        if !(c.is_alphanumeric() || c == '_') {
            return None;
        }
        end += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_simple_statements() {
        let got = split("SELECT 1; SELECT 2;");
        assert_eq!(got, vec!["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn drops_trailing_empty_and_no_trailing_semicolon() {
        let got = split("SELECT 1;   ");
        assert_eq!(got, vec!["SELECT 1"]);
        let got = split("SELECT 1");
        assert_eq!(got, vec!["SELECT 1"]);
    }

    #[test]
    fn ignores_semicolon_in_single_quotes() {
        let got = split("SELECT 'a;b'; SELECT 2;");
        assert_eq!(got, vec!["SELECT 'a;b'", "SELECT 2"]);
    }

    #[test]
    fn handles_escaped_single_quote() {
        let got = split("SELECT 'it''s; fine'; SELECT 2;");
        assert_eq!(got, vec!["SELECT 'it''s; fine'", "SELECT 2"]);
    }

    #[test]
    fn ignores_semicolon_in_double_quotes() {
        let got = split(r#"SELECT "weird;name" FROM t; SELECT 2;"#);
        assert_eq!(got, vec![r#"SELECT "weird;name" FROM t"#, "SELECT 2"]);
    }

    #[test]
    fn ignores_semicolon_in_line_comment() {
        let got = split("SELECT 1; -- a; b\nSELECT 2;");
        assert_eq!(got, vec!["SELECT 1", "-- a; b\nSELECT 2"]);
    }

    #[test]
    fn ignores_semicolon_in_block_comment() {
        let got = split("SELECT 1; /* a; b */ SELECT 2;");
        assert_eq!(got, vec!["SELECT 1", "/* a; b */ SELECT 2"]);
    }

    #[test]
    fn handles_function_body_with_dollar_quoting() {
        let sql = "CREATE FUNCTION f() RETURNS int AS $$ \
                    BEGIN \
                      SELECT 1; SELECT 2; \
                      RETURN 1; \
                    END; \
                    $$ LANGUAGE plpgsql; \
                    SELECT 3;";
        let got = split(sql);
        assert_eq!(got.len(), 2);
        assert!(got[0].contains("SELECT 1; SELECT 2;"));
        assert!(got[0].trim_end().ends_with("LANGUAGE plpgsql"));
        assert_eq!(got[1], "SELECT 3");
    }

    #[test]
    fn block_comments_nest() {
        // PostgreSQL block comments nest: `/* outer /* inner */ still
        // comment */` is ONE comment, not two. A single-level implementation
        // closes the comment at the first `*/` (right after "inner"),
        // leaving the semicolon that follows as ordinary top-level text and
        // splitting what should be one statement into a garbled pair.
        let sql = "/* outer /* inner */ still comment; with semicolon */ SELECT 1;";
        let got = split(sql);
        assert_eq!(
            got,
            vec!["/* outer /* inner */ still comment; with semicolon */ SELECT 1"]
        );
    }

    #[test]
    fn handles_tagged_dollar_quoting() {
        let sql =
            "CREATE FUNCTION f() RETURNS int AS $body$ SELECT 1; $body$ LANGUAGE sql; SELECT 2;";
        let got = split(sql);
        assert_eq!(got.len(), 2);
        assert!(got[0].contains("$body$ SELECT 1; $body$"));
        assert_eq!(got[1], "SELECT 2");
    }
}
