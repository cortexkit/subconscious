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
            // A trailing comma follows a value, never an opening delimiter,
            // colon, or another comma. Leave invalid commas for the JSON reader
            // to refuse instead of repairing an empty/missing element.
            if matches!(lookahead.peek(), Some('}' | ']'))
                && out
                    .trim_end()
                    .chars()
                    .next_back()
                    .is_some_and(|previous| !matches!(previous, '[' | '{' | ':' | ','))
            {
                continue;
            }
        }

        out.push(ch);
    }

    out
}

/// The byte range of one object in the original JSONC document.
///
/// The offsets point into the unmodified document so a caller can insert a
/// member without rewriting comments or whitespace around it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JsoncObjectSpan {
    pub closing_brace: usize,
    pub is_empty: bool,
    pub last_member_end: Option<usize>,
    pub has_trailing_comma: bool,
}

/// Finds the byte range to remove an object member at a path of JSON keys.
/// The range includes one separator comma, leaving other member bytes intact.
/// A final member with a trailing comma includes both its leading and trailing
/// commas when another member precedes it.
pub fn jsonc_member_span(
    doc: &str,
    path: &[&str],
) -> Result<Option<std::ops::Range<usize>>, String> {
    let Some((key, parents)) = path.split_last() else {
        return Ok(None);
    };
    let tokens = tokenize(doc)?;
    let mut parser = Parser::new(&tokens);
    let root = parser.parse_value()?;
    if parser.next().is_some() {
        return Err("unexpected content after the root JSON value".to_string());
    }
    let Some(mut object) = root.object else {
        return Ok(None);
    };
    for segment in parents {
        let Some(member) = object.members.iter().find(|member| member.key == *segment) else {
            return Ok(None);
        };
        let Some(child) = &member.object else {
            return Ok(None);
        };
        object = child.clone();
    }
    let Some((index, member)) = object
        .members
        .iter()
        .enumerate()
        .find(|(_, member)| member.key == *key)
    else {
        return Ok(None);
    };
    let last = index + 1 == object.members.len();
    Ok(Some(
        match (member.comma_before, member.comma_after, last) {
            (Some(start), Some(end), true) => start..end,
            (Some(start), None, _) => start..member.value_end,
            (_, Some(end), _) => member.key_start..end,
            (None, None, _) => member.key_start..member.value_end,
        },
    ))
}

/// Finds an object's closing brace by key path while treating comments and
/// strings as lexical content rather than syntax. This is useful for editors
/// that must preserve the original JSONC bytes around an added member.
pub fn jsonc_object_span(doc: &str, path: &[&str]) -> Result<Option<JsoncObjectSpan>, String> {
    let tokens = tokenize(doc)?;
    let mut parser = Parser::new(&tokens);
    let root = parser.parse_value()?;
    if parser.next().is_some() {
        return Err("unexpected content after the root JSON value".to_string());
    }
    let Some(mut object) = root.object else {
        return Ok(None);
    };
    for segment in path {
        let Some(member) = object.members.iter().find(|member| member.key == *segment) else {
            return Ok(None);
        };
        let Some(child) = &member.object else {
            return Ok(None);
        };
        object = child.clone();
    }
    Ok(Some(JsoncObjectSpan {
        closing_brace: object.closing_brace,
        is_empty: object.members.is_empty(),
        last_member_end: object.members.last().map(|member| member.value_end),
        has_trailing_comma: object.has_trailing_comma,
    }))
}

#[derive(Clone, Debug)]
struct Token {
    kind: TokenKind,
    start: usize,
    end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum TokenKind {
    LeftBrace,
    RightBrace,
    LeftBracket,
    RightBracket,
    Colon,
    Comma,
    String(String),
    Atom,
}

fn tokenize(doc: &str) -> Result<Vec<Token>, String> {
    let bytes = doc.as_bytes();
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        match bytes[cursor] {
            byte if byte.is_ascii_whitespace() => cursor += 1,
            b'/' if bytes.get(cursor + 1) == Some(&b'/') => {
                cursor += 2;
                while cursor < bytes.len() && bytes[cursor] != b'\n' {
                    cursor += 1;
                }
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                let start = cursor;
                cursor += 2;
                while cursor + 1 < bytes.len()
                    && !(bytes[cursor] == b'*' && bytes[cursor + 1] == b'/')
                {
                    cursor += 1;
                }
                if cursor + 1 >= bytes.len() {
                    return Err(format!("unterminated block comment at byte {start}"));
                }
                cursor += 2;
            }
            b'"' => {
                let start = cursor;
                cursor += 1;
                let mut escaped = false;
                let mut closed = false;
                while cursor < bytes.len() {
                    match bytes[cursor] {
                        _ if escaped => escaped = false,
                        b'\\' => escaped = true,
                        b'"' => {
                            cursor += 1;
                            closed = true;
                            break;
                        }
                        _ => {}
                    }
                    cursor += 1;
                }
                if !closed {
                    return Err(format!("unterminated string at byte {start}"));
                }
                let value = serde_json::from_str::<String>(&doc[start..cursor])
                    .map_err(|error| format!("invalid JSON string at byte {start}: {error}"))?;
                tokens.push(Token {
                    kind: TokenKind::String(value),
                    start,
                    end: cursor,
                });
            }
            byte @ (b'{' | b'}' | b'[' | b']' | b':' | b',') => {
                let kind = match byte {
                    b'{' => TokenKind::LeftBrace,
                    b'}' => TokenKind::RightBrace,
                    b'[' => TokenKind::LeftBracket,
                    b']' => TokenKind::RightBracket,
                    b':' => TokenKind::Colon,
                    b',' => TokenKind::Comma,
                    _ => unreachable!("the match only accepts JSON punctuation"),
                };
                tokens.push(Token {
                    kind,
                    start: cursor,
                    end: cursor + 1,
                });
                cursor += 1;
            }
            _ => {
                let start = cursor;
                while cursor < bytes.len()
                    && !bytes[cursor].is_ascii_whitespace()
                    && !matches!(
                        bytes[cursor],
                        b'{' | b'}' | b'[' | b']' | b':' | b',' | b'"' | b'/'
                    )
                {
                    cursor += 1;
                }
                if start == cursor {
                    return Err(format!("unexpected byte at {cursor}"));
                }
                tokens.push(Token {
                    kind: TokenKind::Atom,
                    start,
                    end: cursor,
                });
            }
        }
    }
    Ok(tokens)
}

#[derive(Clone, Debug)]
struct ParsedValue {
    end: usize,
    object: Option<ParsedObject>,
}

#[derive(Clone, Debug)]
struct ParsedObject {
    closing_brace: usize,
    members: Vec<ParsedMember>,
    has_trailing_comma: bool,
}

#[derive(Clone, Debug)]
struct ParsedMember {
    key: String,
    key_start: usize,
    value_end: usize,
    comma_before: Option<usize>,
    comma_after: Option<usize>,
    object: Option<ParsedObject>,
}

struct Parser<'a> {
    tokens: &'a [Token],
    cursor: usize,
}

impl<'a> Parser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self { tokens, cursor: 0 }
    }

    fn next(&mut self) -> Option<&'a Token> {
        let token = self.tokens.get(self.cursor)?;
        self.cursor += 1;
        Some(token)
    }

    fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.cursor)
    }

    fn parse_value(&mut self) -> Result<ParsedValue, String> {
        let token = self
            .next()
            .ok_or_else(|| "expected a JSON value".to_string())?;
        match &token.kind {
            TokenKind::LeftBrace => self.parse_object(),
            TokenKind::LeftBracket => self.parse_array(),
            TokenKind::String(_) | TokenKind::Atom => Ok(ParsedValue {
                end: token.end,
                object: None,
            }),
            _ => Err(format!("expected a JSON value at byte {}", token.start)),
        }
    }

    fn parse_object(&mut self) -> Result<ParsedValue, String> {
        let mut members = Vec::new();
        let mut has_trailing_comma = false;
        let mut previous_comma = None;
        if matches!(
            self.peek().map(|token| &token.kind),
            Some(TokenKind::RightBrace)
        ) {
            let close = self.next().expect("peeked token is present");
            return Ok(ParsedValue {
                end: close.end,
                object: Some(ParsedObject {
                    closing_brace: close.start,
                    members,
                    has_trailing_comma,
                }),
            });
        }
        loop {
            let (key, key_start) = match self.next() {
                Some(Token {
                    kind: TokenKind::String(key),
                    start,
                    ..
                }) => (key.clone(), *start),
                Some(token) => {
                    return Err(format!("expected an object key at byte {}", token.start))
                }
                None => return Err("unterminated object".to_string()),
            };
            let colon = self
                .next()
                .ok_or_else(|| "object key is missing a colon".to_string())?;
            if !matches!(colon.kind, TokenKind::Colon) {
                return Err(format!("expected a colon at byte {}", colon.start));
            }
            let value = self.parse_value()?;
            members.push(ParsedMember {
                key,
                key_start,
                value_end: value.end,
                comma_before: previous_comma,
                comma_after: None,
                object: value.object,
            });
            match self.next() {
                Some(Token {
                    kind: TokenKind::RightBrace,
                    start,
                    end,
                }) => {
                    return Ok(ParsedValue {
                        end: *end,
                        object: Some(ParsedObject {
                            closing_brace: *start,
                            members,
                            has_trailing_comma,
                        }),
                    })
                }
                Some(Token {
                    kind: TokenKind::Comma,
                    start,
                    end,
                }) => {
                    members
                        .last_mut()
                        .expect("member was just appended")
                        .comma_after = Some(*end);
                    previous_comma = Some(*start);
                    if matches!(
                        self.peek().map(|token| &token.kind),
                        Some(TokenKind::RightBrace)
                    ) {
                        has_trailing_comma = true;
                        let close = self.next().expect("peeked token is present");
                        return Ok(ParsedValue {
                            end: close.end,
                            object: Some(ParsedObject {
                                closing_brace: close.start,
                                members,
                                has_trailing_comma,
                            }),
                        });
                    }
                }
                Some(token) => {
                    return Err(format!(
                        "expected a comma or closing brace at byte {}",
                        token.start
                    ))
                }
                None => return Err("unterminated object".to_string()),
            }
        }
    }

    fn parse_array(&mut self) -> Result<ParsedValue, String> {
        if matches!(
            self.peek().map(|token| &token.kind),
            Some(TokenKind::RightBracket)
        ) {
            let close = self.next().expect("peeked token is present");
            return Ok(ParsedValue {
                end: close.end,
                object: None,
            });
        }
        loop {
            self.parse_value()?;
            match self.next() {
                Some(Token {
                    kind: TokenKind::RightBracket,
                    end,
                    ..
                }) => {
                    return Ok(ParsedValue {
                        end: *end,
                        object: None,
                    })
                }
                Some(Token {
                    kind: TokenKind::Comma,
                    ..
                }) if matches!(
                    self.peek().map(|token| &token.kind),
                    Some(TokenKind::RightBracket)
                ) =>
                {
                    let close = self.next().expect("peeked token is present");
                    return Ok(ParsedValue {
                        end: close.end,
                        object: None,
                    });
                }
                Some(Token {
                    kind: TokenKind::Comma,
                    ..
                }) => {}
                Some(token) => {
                    return Err(format!(
                        "expected a comma or closing bracket at byte {}",
                        token.start
                    ))
                }
                None => return Err("unterminated array".to_string()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn remove_member(doc: &str, path: &[&str]) -> String {
        let span = jsonc_member_span(doc, path)
            .unwrap()
            .expect("member exists");
        let mut updated = doc.to_string();
        updated.replace_range(span, "");
        jsonc_to_json(&updated)
            .and_then(|json| {
                serde_json::from_str::<Value>(&json).map_err(|error| error.to_string())
            })
            .expect("removal keeps document valid");
        updated
    }

    #[test]
    fn member_span_removes_first_middle_and_last_with_the_correct_comma() {
        let doc = r#"{"first": 1, "middle": 2, "last": 3}"#;
        assert_eq!(
            remove_member(doc, &["first"]),
            r#"{ "middle": 2, "last": 3}"#
        );
        assert_eq!(
            remove_member(doc, &["middle"]),
            r#"{"first": 1,  "last": 3}"#
        );
        assert_eq!(
            remove_member(doc, &["last"]),
            r#"{"first": 1, "middle": 2}"#
        );
    }

    #[test]
    fn member_span_handles_trailing_comment_and_nested_object() {
        let doc = "{\n  // retain root note\n  \"modules\": {\n    \"aft\": {\"program\": \"old\"} // belongs to aft\n    , \"mc\": {\"program\": \"keep\"}\n  }\n}";
        let updated = remove_member(doc, &["modules", "aft"]);
        assert!(updated.contains("// retain root note"));
        assert!(!updated.contains("// belongs to aft"));
        let parsed: Value = serde_json::from_str(&jsonc_to_json(&updated).unwrap()).unwrap();
        assert!(parsed.pointer("/modules/aft").is_none());
        assert_eq!(parsed.pointer("/modules/mc/program"), Some(&json!("keep")));
        assert!(jsonc_member_span(doc, &["modules", "missing"])
            .unwrap()
            .is_none());
    }

    #[test]
    fn member_span_ignores_punctuation_and_comments_inside_string_values() {
        let doc = r#"{"first": "// /* , } \"escaped\"", "second": 2}"#;
        let updated = remove_member(doc, &["first"]);
        assert_eq!(updated, r#"{ "second": 2}"#);
        let updated = remove_member(doc, &["second"]);
        assert!(updated.contains(r#"// /* , }"#));
    }

    #[test]
    fn member_span_removes_final_trailing_comma_without_leaving_two_separators() {
        assert_eq!(remove_member(r#"{"a": 1, "b": 2,}"#, &["b"]), r#"{"a": 1}"#);
        assert_eq!(remove_member(r#"{"only": 1,}"#, &["only"]), "{}");
    }

    #[test]
    fn object_span_skips_comments_and_string_literals() {
        let doc = r#"{
  // root comment
  "modules": {
    /* a brace } inside a comment */
    "aft": { "program": "literal } // not syntax" },
  },
}"#;
        let span = jsonc_object_span(doc, &["modules"])
            .expect("parse")
            .expect("modules object");
        assert!(!span.is_empty);
        assert!(span.has_trailing_comma);
        assert_eq!(&doc[span.closing_brace..=span.closing_brace], "}");
    }

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
    fn editor_helpers_refuse_block_comment_openers_at_eof() {
        for doc in ["{} /*", "{} /*x", "{} /*\n", "{} /**"] {
            assert!(jsonc_to_json(doc).is_err());
            let error = jsonc_object_span(doc, &[]).expect_err(doc);
            assert!(error.contains("unterminated block comment"), "{error}");
            assert!(jsonc_member_span(doc, &["x"]).is_err());
        }
        assert!(jsonc_object_span("{} /**/", &[]).unwrap().is_some());
    }

    #[test]
    fn normalization_does_not_turn_lone_commas_into_valid_json() {
        for doc in [
            "[,]",
            "{,}",
            "[/*comment*/,]",
            "{ /*comment*/ , }",
            "[1,,]",
            r#"{"x":,}"#,
        ] {
            let normalized = jsonc_to_json(doc).unwrap();
            assert!(
                serde_json::from_str::<Value>(&normalized).is_err(),
                "invalid {doc} was repaired to {normalized}"
            );
            assert!(jsonc_object_span(doc, &[]).is_err());
        }
        for doc in ["[1,]", r#"{"x": [],}"#, "[{},]", "[true,]", "[null,]"] {
            assert!(
                serde_json::from_str::<Value>(&jsonc_to_json(doc).unwrap()).is_ok(),
                "{doc}"
            );
        }
    }

    #[test]
    fn rejects_unterminated_string() {
        assert_eq!(
            jsonc_to_json(r#"{ "text": "unterminated }"#),
            Err("unterminated string".to_owned())
        );
    }
}
