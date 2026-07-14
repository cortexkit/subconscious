#![forbid(unsafe_code)]

/// Normalize a JSONC document into strict JSON by stripping comments and
/// trailing commas while preserving string literals verbatim.
pub fn jsonc_to_json(doc: &str) -> Result<String, String> {
    let mut out = String::with_capacity(doc.len());
    let mut chars = doc.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        // A removed comment is lexical whitespace; emit a space so adjacent
        // tokens cannot merge. Newlines are preserved for error-line fidelity.
        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            '/' if chars.peek() == Some(&'/') => {
                let _ = chars.next();
                out.push(' ');
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                let _ = chars.next();
                out.push(' ');
                let mut closed = false;
                let mut prev = '\0';
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                    }
                    if prev == '*' && next == '/' {
                        closed = true;
                        break;
                    }
                    prev = next;
                }
                if !closed {
                    return Err("unterminated block comment".to_owned());
                }
            }
            _ => out.push(ch),
        }
    }

    if in_string {
        return Err("unterminated string".to_owned());
    }

    Ok(remove_json_trailing_commas(&out))
}

fn remove_json_trailing_commas(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            out.push(ch);
            continue;
        }

        if ch == ',' {
            let mut lookahead = chars.clone();
            while matches!(lookahead.peek(), Some(next) if next.is_whitespace()) {
                let _ = lookahead.next();
            }
            if matches!(lookahead.peek(), Some('}' | ']')) {
                continue;
            }
        }

        out.push(ch);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn jsonc_matrix_normalizes_comments_trailing_commas_and_string_literals() {
        let cases = [
            (
                "line_comment_and_object_trailing_comma",
                r#"
                {
                  // keep this object valid after stripping comments
                  "version": 1,
                  "name": "aft",
                }
                "#,
                json!({
                    "version": 1,
                    "name": "aft",
                }),
            ),
            (
                "block_comments_and_array_trailing_comma",
                r#"
                {
                  /* drop the whole block */
                  "ports": [1, 2, 3,],
                  "enabled": true,
                }
                "#,
                json!({
                    "ports": [1, 2, 3],
                    "enabled": true,
                }),
            ),
            (
                "strings_preserve_comment_markers_commas_and_escaped_quotes",
                r#"
                {
                  "text": "literal // comment, comma, and \"quote\" stay",
                  "items": ["a,//", "b,/*still string*/",],
                }
                "#,
                json!({
                    "text": "literal // comment, comma, and \"quote\" stay",
                    "items": ["a,//", "b,/*still string*/"],
                }),
            ),
            (
                "nested_objects_and_arrays",
                r#"
                {
                  "outer": {
                    "inner": [
                      { "name": "one", },
                      { /* nested */ "name": "two" },
                    ],
                  },
                }
                "#,
                json!({
                    "outer": {
                        "inner": [
                            { "name": "one" },
                            { "name": "two" },
                        ],
                    },
                }),
            ),
        ];

        for (name, doc, expected) in cases {
            let normalized = jsonc_to_json(doc).unwrap_or_else(|err| panic!("{name}: {err}"));
            let actual: Value = serde_json::from_str(&normalized)
                .unwrap_or_else(|err| panic!("{name}: invalid json output: {err}"));
            assert_eq!(actual, expected, "{name}");
        }
    }

    #[test]
    fn well_formed_block_comment_preserves_newlines_and_value() {
        let doc = "{\"a\":1,/* first\nsecond */\"b\":2,}";
        let normalized = jsonc_to_json(doc).unwrap();
        let actual: Value = serde_json::from_str(&normalized).unwrap();

        assert_eq!(normalized.matches('\n').count(), doc.matches('\n').count());
        assert_eq!(actual, json!({ "a": 1, "b": 2 }));
    }

    #[test]
    fn block_comment_cannot_merge_adjacent_tokens() {
        let normalized = jsonc_to_json(r#"{"version":1,"port":8/* digits */123}"#).unwrap();

        assert_eq!(normalized, r#"{"version":1,"port":8 123}"#);
        assert!(serde_json::from_str::<Value>(&normalized).is_err());
    }

    #[test]
    fn rejects_unterminated_block_comment() {
        assert_eq!(
            jsonc_to_json(r#"{ "version": 1, /*"#),
            Err("unterminated block comment".to_owned())
        );
    }

    #[test]
    fn rejects_unterminated_string() {
        assert_eq!(
            jsonc_to_json(r#"{ "text": "unterminated }"#),
            Err("unterminated string".to_owned())
        );
    }
}
