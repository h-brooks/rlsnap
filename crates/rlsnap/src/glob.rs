//! Minimal `*`-wildcard matching for schema/table exclude patterns
//! (e.g. `"storage.*"`, `"auth.*"`).

/// Does `pattern` (containing zero or more `*` wildcards, each matching any
/// run of characters) match `text` in full?
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let mut regex_src = String::from("^");
    for ch in pattern.chars() {
        if ch == '*' {
            regex_src.push_str(".*");
        } else if "\\.+?()|[]{}^$".contains(ch) {
            regex_src.push('\\');
            regex_src.push(ch);
        } else {
            regex_src.push(ch);
        }
    }
    regex_src.push('$');
    regex::Regex::new(&regex_src)
        .map(|re| re.is_match(text))
        .unwrap_or(false)
}

/// Does `text` match any pattern in `patterns`?
pub fn any_match(patterns: &[String], text: &str) -> bool {
    patterns.iter().any(|p| glob_match(p, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        assert!(glob_match("public.widgets", "public.widgets"));
        assert!(!glob_match("public.widgets", "public.gadgets"));
    }

    #[test]
    fn wildcard_matches_schema_prefix() {
        assert!(glob_match("storage.*", "storage.objects"));
        assert!(!glob_match("storage.*", "public.objects"));
    }

    #[test]
    fn wildcard_alone_matches_everything() {
        assert!(glob_match("*", "anything.at.all"));
    }

    #[test]
    fn any_match_checks_all_patterns() {
        let patterns = vec!["storage.*".to_string(), "auth.*".to_string()];
        assert!(any_match(&patterns, "auth.users"));
        assert!(!any_match(&patterns, "public.widgets"));
    }
}
