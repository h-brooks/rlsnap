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

/// Does `text` match any pattern in `patterns`, for `accept --only`? A
/// pattern containing `*` is matched as a glob (the whole of `text` must
/// match, same as [`any_match`]); a pattern with no `*` is matched as a
/// plain substring, so `--only grant_billing_export_access` hits the function
/// signature `public.grant_billing_export_access(text)` without the caller
/// having to spell out the full signature or escape anything.
pub fn only_match(patterns: &[String], text: &str) -> bool {
    patterns.iter().any(|p| {
        if p.contains('*') {
            glob_match(p, text)
        } else {
            text.contains(p.as_str())
        }
    })
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

    #[test]
    fn only_match_plain_pattern_is_a_substring_match() {
        let patterns = vec!["grant_billing_export_access".to_string()];
        assert!(only_match(
            &patterns,
            "public.grant_billing_export_access(text)"
        ));
        assert!(!only_match(&patterns, "public.other_function(text)"));
    }

    #[test]
    fn only_match_glob_pattern_requires_a_full_match() {
        let patterns = vec!["public.set_*".to_string()];
        assert!(only_match(&patterns, "public.set_flag(boolean)"));
        // A glob pattern is a full match, unlike a plain pattern's
        // substring match: "public.set_*" does not match a signature that
        // merely contains "set_" somewhere in the middle.
        assert!(!only_match(&patterns, "public.reset_flag(boolean)"));
    }

    #[test]
    fn only_match_checks_all_patterns() {
        let patterns = vec!["storage.*".to_string(), "widgets".to_string()];
        assert!(only_match(&patterns, "public.widgets"));
        assert!(only_match(&patterns, "storage.objects"));
        assert!(!only_match(&patterns, "public.gadgets"));
    }
}
