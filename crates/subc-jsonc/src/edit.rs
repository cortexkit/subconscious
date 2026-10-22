//! Span-preserving edits to a JSONC document.
//!
//! Each edit changes only the bytes of the value it targets. Adding a member
//! or element also adds one separator comma and the new text (on a line of its
//! own when its container is written one entry per line); removing one also
//! removes one separator comma and any line the removal leaves holding only
//! whitespace. Comments, blank lines, indentation, CRLF line endings and
//! trailing commas elsewhere in the document stay byte-for-byte as they were.
//! A value that is replaced or removed takes the comments written inside it
//! along with it.

use std::{fs, ops::Range, path::Path};

use serde_json::Value;

use super::{
    jsonc_object_span, jsonc_to_json, tokenize, ParsedArray, ParsedElement, ParsedMember,
    ParsedObject, Parser,
};

/// Reads and parses an existing JSONC file. A missing file is an error here:
/// callers edit a configuration the operator already has.
pub fn read_document(path: &Path) -> Result<(String, Value), String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    let strict = jsonc_to_json(&text)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    let document = serde_json::from_str::<Value>(&strict)
        .map_err(|error| format!("could not parse {}: {error}", path.display()))?;
    Ok((text, document))
}

/// Adds `key` as the last member of the object at `parent_path`.
///
/// The parent must exist and must not already hold `key`. The new member is
/// indented like the parent's closing brace plus one indentation unit, uses
/// the document's newline style, and a separator comma is added after the
/// previous last member. A line comment after that member stays after it.
pub fn insert_object_member(
    document: &mut String,
    parent_path: &[&str],
    key: &str,
    value: &Value,
) -> Result<(), String> {
    insert_object_member_with(document, parent_path, key, value, |_, _| {})
}

/// [`insert_object_member`], with `decorate` called on the rendered member
/// text (`"key": value`, without its leading indentation) and the document's
/// newline before the member is inserted. A caller uses it to place comments
/// inside the member it adds.
pub fn insert_object_member_with(
    document: &mut String,
    parent_path: &[&str],
    key: &str,
    value: &Value,
    decorate: impl FnOnce(&mut String, &str),
) -> Result<(), String> {
    let mut object = jsonc_object_span(document, parent_path)?
        .ok_or_else(|| "parent object is absent".to_string())?;
    if !object.is_empty && !object.has_trailing_comma {
        let last_value_end = object
            .last_member_end
            .ok_or_else(|| "non-empty object has no final member".to_string())?;
        // A line comment may follow the value. Put the separator before that
        // comment so the comment continues to describe the existing member.
        document.insert(last_value_end, ',');
        object = jsonc_object_span(document, parent_path)?
            .ok_or_else(|| "parent object disappeared".to_string())?;
    }

    let newline = newline_of(document);
    let indentation = indentation_unit(document);
    let closing_line_start = line_start(document, object.closing_brace);
    let closing_indent = &document[closing_line_start..object.closing_brace];
    let closing_has_only_indent = closing_indent
        .chars()
        .all(|character| matches!(character, ' ' | '\t' | '\r'));
    let member_indent = if closing_has_only_indent {
        format!("{closing_indent}{indentation}")
    } else {
        indentation.repeat(parent_path.len() + 1)
    };
    let mut member = render_member(key, value, &member_indent, &indentation, newline);
    decorate(&mut member, newline);

    if closing_has_only_indent {
        document.insert_str(
            closing_line_start,
            &format!("{member_indent}{member}{newline}"),
        );
    } else {
        // The closing brace shares its line with content, so the member goes on
        // a new line after that content. Any spaces before the brace would be
        // left at the end of that line, so they go.
        let mut content_end = object.closing_brace;
        while content_end > 0 && matches!(document.as_bytes()[content_end - 1], b' ' | b'\t') {
            content_end -= 1;
        }
        let closing_indent = indentation.repeat(parent_path.len());
        document.replace_range(
            content_end..object.closing_brace,
            &format!("{newline}{member_indent}{member}{newline}{closing_indent}"),
        );
    }
    Ok(())
}

/// Adds the absent member at `path`, creating any absent parent objects.
///
/// The first absent key on the path is added with [`insert_object_member`] to
/// the deepest object that exists, and its value nests the rest of the path.
/// A member that already exists, or a parent on the path that exists but is
/// not an object, is refused.
pub fn insert_member(document: &mut String, path: &[&str], value: &Value) -> Result<(), String> {
    insert_member_with(document, path, value, |_, _| {})
}

/// [`insert_member`], with `decorate` called on the rendered member text as
/// in [`insert_object_member_with`]. When parents are created, the rendered
/// member is the outermost created parent.
pub fn insert_member_with(
    document: &mut String,
    path: &[&str],
    value: &Value,
    decorate: impl FnOnce(&mut String, &str),
) -> Result<(), String> {
    let root = parse_root(document)?;
    match lookup(&root, path)? {
        Lookup::Found { .. } => Err(format!("{} already exists", path.join("."))),
        Lookup::Missing { depth } => insert_object_member_with(
            document,
            &path[..depth],
            path[depth],
            &nest(&path[depth + 1..], value),
            decorate,
        ),
    }
}

/// Sets the member at `path` to `value`, for example a module's `enabled`
/// flag or one of its environment variables.
///
/// An existing member keeps its key and every byte outside its value; only the
/// value's bytes are replaced, so comments inside an object or array value go
/// with it. An absent member is added with [`insert_member`].
pub fn set_member(document: &mut String, path: &[&str], value: &Value) -> Result<(), String> {
    let root = parse_root(document)?;
    match lookup(&root, path)? {
        Lookup::Found { member, .. } => {
            let rendered = render_in_place(document, member.key_start, value);
            document.replace_range(member.value_start..member.value_end, &rendered);
            Ok(())
        }
        Lookup::Missing { .. } => insert_member(document, path, value),
    }
}

/// Removes the member at `path` with one separator comma. Returns `false`,
/// changing nothing, when the member is absent.
///
/// Comments outside the member's value stay, and a line left holding only
/// whitespace is removed with its newline.
pub fn remove_member(document: &mut String, path: &[&str]) -> Result<bool, String> {
    let root = parse_root(document)?;
    let Lookup::Found { member, parent } = lookup(&root, path)? else {
        return Ok(false);
    };
    let is_last = parent
        .members
        .last()
        .is_some_and(|last| last.key_start == member.key_start);
    remove_entry(
        document,
        member.key_start..member.value_end,
        member.comma_before,
        // The parser records the end of the comma after a member.
        member.comma_after.map(|end| end - 1),
        is_last,
    );
    Ok(true)
}

/// Replaces the element at `index` of the array at `path`, for example one of
/// a module's program arguments.
pub fn set_element(
    document: &mut String,
    path: &[&str],
    index: usize,
    value: &Value,
) -> Result<(), String> {
    let root = parse_root(document)?;
    let array = array_at(&root, path)?.ok_or_else(|| format!("{} is absent", path.join(".")))?;
    let element = element_at(array, path, index)?;
    let rendered = render_in_place(document, element.start, value);
    document.replace_range(element.start..element.end, &rendered);
    Ok(())
}

/// Appends `value` to the array at `path`, creating the array (and absent
/// parent objects) when it is absent.
///
/// An array written one element per line gets the new element on its own line
/// before the closing bracket, indented like the last element. An array
/// written on one line gets `, value` after its last element.
pub fn push_element(document: &mut String, path: &[&str], value: &Value) -> Result<(), String> {
    let root = parse_root(document)?;
    let Some(array) = array_at(&root, path)? else {
        return insert_member(document, path, &Value::Array(vec![value.clone()]));
    };
    let newline = newline_of(document);
    let unit = indentation_unit(document);
    let closing_line_start = line_start(document, array.closing_bracket);
    let closing_indent = document[closing_line_start..array.closing_bracket].to_string();
    let closing_on_own_line = closing_line_start > array.opening_bracket
        && closing_indent
            .chars()
            .all(|character| matches!(character, ' ' | '\t'));
    let compact = serde_json::to_string(&sorted_keys(value)).expect("JSON values always serialize");
    match array.elements.last() {
        None if closing_on_own_line => {
            let element_indent = format!("{closing_indent}{unit}");
            let rendered = render_value(value, &element_indent, &unit, newline);
            document.insert_str(
                closing_line_start,
                &format!("{element_indent}{rendered}{newline}"),
            );
        }
        None => document.insert_str(array.closing_bracket, &compact),
        Some(last) if closing_on_own_line => {
            let last_line_start = line_start(document, last.start);
            let before_last = &document[last_line_start..last.start];
            let element_indent = if before_last
                .chars()
                .all(|character| matches!(character, ' ' | '\t'))
            {
                before_last.to_string()
            } else {
                format!("{closing_indent}{unit}")
            };
            let rendered = render_value(value, &element_indent, &unit, newline);
            let mut at = closing_line_start;
            if !array.has_trailing_comma {
                // Before any comment that follows the last element, as for
                // object members.
                document.insert(last.end, ',');
                at += 1;
            }
            document.insert_str(at, &format!("{element_indent}{rendered}{newline}"));
        }
        Some(last) => match last.comma_after {
            Some(trailing_comma) => {
                document.insert_str(trailing_comma + 1, &format!(" {compact}"));
            }
            None => document.insert_str(last.end, &format!(", {compact}")),
        },
    }
    Ok(())
}

/// Removes the element at `index` of the array at `path` with one separator
/// comma, keeping comments outside the element as [`remove_member`] does.
pub fn remove_element(document: &mut String, path: &[&str], index: usize) -> Result<(), String> {
    let root = parse_root(document)?;
    let array = array_at(&root, path)?.ok_or_else(|| format!("{} is absent", path.join(".")))?;
    let element = element_at(array, path, index)?;
    remove_entry(
        document,
        element.start..element.end,
        element.comma_before,
        element.comma_after,
        index + 1 == array.elements.len(),
    );
    Ok(())
}

/// Where a path leads in a parsed document.
enum Lookup<'a> {
    Found {
        member: &'a ParsedMember,
        /// The object holding `member`.
        parent: &'a ParsedObject,
    },
    /// `path[..depth]` names an object that exists and has no `path[depth]`.
    Missing { depth: usize },
}

fn lookup<'a>(root: &'a ParsedObject, path: &[&str]) -> Result<Lookup<'a>, String> {
    if path.is_empty() {
        return Err("an empty path names no member".to_string());
    }
    let mut object = root;
    for (depth, segment) in path.iter().enumerate() {
        let Some(member) = object.members.iter().find(|member| member.key == *segment) else {
            return Ok(Lookup::Missing { depth });
        };
        if depth + 1 == path.len() {
            return Ok(Lookup::Found {
                member,
                parent: object,
            });
        }
        object = member
            .object
            .as_ref()
            .ok_or_else(|| format!("{} is not an object", path[..=depth].join(".")))?;
    }
    unreachable!("a non-empty path ends at its last segment")
}

fn array_at<'a>(root: &'a ParsedObject, path: &[&str]) -> Result<Option<&'a ParsedArray>, String> {
    match lookup(root, path)? {
        Lookup::Found { member, .. } => member
            .array
            .as_ref()
            .map(Some)
            .ok_or_else(|| format!("{} is not an array", path.join("."))),
        Lookup::Missing { .. } => Ok(None),
    }
}

fn element_at<'a>(
    array: &'a ParsedArray,
    path: &[&str],
    index: usize,
) -> Result<&'a ParsedElement, String> {
    array.elements.get(index).ok_or_else(|| {
        format!(
            "{} has {} elements, so there is no element {index}",
            path.join("."),
            array.elements.len()
        )
    })
}

fn parse_root(document: &str) -> Result<ParsedObject, String> {
    let tokens = tokenize(document)?;
    let mut parser = Parser::new(&tokens);
    let root = parser.parse_value()?;
    if parser.next().is_some() {
        return Err("unexpected content after the root JSON value".to_string());
    }
    root.object
        .ok_or_else(|| "the document's root is not an object".to_string())
}

/// `value` nested under `keys`, outermost key first.
fn nest(keys: &[&str], value: &Value) -> Value {
    keys.iter().rev().fold(value.clone(), |inner, key| {
        Value::Object([((*key).to_string(), inner)].into_iter().collect())
    })
}

/// Removes one entry (an object member or an array element) spanning `entry`,
/// and one separator: the comma after it, or the comma before it when there
/// is none. A last entry that has both a comma before it and a trailing comma
/// gives up the comma before it instead, so the container keeps its trailing
/// comma; an only entry takes its trailing comma with it. The comma goes
/// with the entry in one cut when only whitespace lies between them, and
/// the spaces after a removed following comma go too; when a comment lies
/// between them, each is cut on its own and the comment stays. Each cut that
/// leaves its line holding only whitespace removes that line as well.
fn remove_entry(
    document: &mut String,
    entry: Range<usize>,
    comma_before: Option<usize>,
    comma_after: Option<usize>,
    is_last: bool,
) {
    let mut cuts = Vec::new();
    let comma_after = comma_after.filter(|_| !(is_last && comma_before.is_some()));
    if let Some(comma) = comma_after {
        let mut after = comma + 1;
        while matches!(document.as_bytes().get(after), Some(b' ' | b'\t')) {
            after += 1;
        }
        if document[entry.end..comma].trim().is_empty() {
            cuts.push(entry.start..after);
        } else {
            cuts.push(entry.clone());
            cuts.push(comma..after);
        }
    } else if let Some(comma) = comma_before {
        if document[comma + 1..entry.start].trim().is_empty() {
            cuts.push(comma..entry.end);
        } else {
            cuts.push(comma..comma + 1);
            cuts.push(entry);
        }
    } else {
        cuts.push(entry);
    }
    // Last cut first, so the earlier cuts' offsets stay valid.
    for mut cut in cuts.into_iter().rev() {
        // A cut that starts its line also takes the spaces after it, so what
        // stays on the line (a comment, say) starts at the line's indentation
        // instead of one space past it.
        let start = line_start(document, cut.start);
        if document[start..cut.start]
            .chars()
            .all(|character| matches!(character, ' ' | '\t'))
        {
            while matches!(document.as_bytes().get(cut.end), Some(b' ' | b'\t')) {
                cut.end += 1;
            }
        }
        let at = cut.start;
        document.replace_range(cut, "");
        let start = line_start(document, at);
        if let Some(newline) = document[at..].find('\n').map(|offset| at + offset) {
            if document[start..newline].trim().is_empty() {
                document.replace_range(start..=newline, "");
            }
        }
    }
}

/// The byte offset where the line holding `at` starts.
fn line_start(document: &str, at: usize) -> usize {
    document[..at].rfind('\n').map_or(0, |index| index + 1)
}

/// The document's newline: CRLF when it has any, else LF.
fn newline_of(document: &str) -> &'static str {
    if document.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// The indentation of the first indented line that has content, or two spaces
/// for a document with none.
fn indentation_unit(document: &str) -> String {
    document
        .lines()
        .find_map(|line| {
            let indent = line
                .chars()
                .take_while(|character| matches!(character, ' ' | '\t'))
                .collect::<String>();
            (!indent.is_empty()
                && line[indent.len()..]
                    .chars()
                    .any(|character| !character.is_whitespace()))
            .then_some(indent)
        })
        .unwrap_or_else(|| "  ".to_string())
}

/// Renders `value` to replace a value whose key or element starts at `at`:
/// continuation lines of an object or array are indented from that line's
/// leading whitespace.
fn render_in_place(document: &str, at: usize, value: &Value) -> String {
    let start = line_start(document, at);
    let line_indent = document[start..at]
        .chars()
        .take_while(|character| matches!(character, ' ' | '\t'))
        .collect::<String>();
    render_value(
        value,
        &line_indent,
        &indentation_unit(document),
        newline_of(document),
    )
}

fn render_member(
    key: &str,
    value: &Value,
    member_indent: &str,
    indentation: &str,
    newline: &str,
) -> String {
    format!(
        "{}: {}",
        serde_json::to_string(key).expect("key encodes"),
        render_value(value, member_indent, indentation, newline)
    )
}

/// Pretty-prints `value` with every object's keys in sorted order.
///
/// serde_json keeps an object's keys sorted by default, but its
/// `preserve_order` feature keeps them in insertion order instead. Cargo
/// builds one `serde_json` for every crate in a build that uses it, so any of
/// those crates can turn the feature on for all of them. Sorting here keeps the
/// text the same however the program was built.
pub fn to_string_pretty_sorted(value: &Value) -> String {
    serde_json::to_string_pretty(&sorted_keys(value)).expect("JSON values always serialize")
}

/// A copy of `value` whose objects hold their keys in sorted order. A map
/// built by inserting sorted keys iterates sorted with or without
/// `preserve_order`.
fn sorted_keys(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries = object.iter().collect::<Vec<_>>();
            entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), sorted_keys(value)))
                    .collect(),
            )
        }
        Value::Array(elements) => Value::Array(elements.iter().map(sorted_keys).collect()),
        other => other.clone(),
    }
}

/// Pretty-prints `value` with sorted keys. Its first line is returned as is;
/// each later line starts on `newline`, indented by `line_indent` plus one
/// `indentation` per nesting level, so the value matches the document's own
/// indentation.
fn render_value(value: &Value, line_indent: &str, indentation: &str, newline: &str) -> String {
    let rendered = to_string_pretty_sorted(value);
    let mut lines = rendered.lines();
    let mut output = lines
        .next()
        .expect("serialized JSON has a first line")
        .to_string();
    for line in lines {
        // serde_json indents its pretty output by two spaces per level.
        let spaces = line.bytes().take_while(|byte| *byte == b' ').count();
        let depth = spaces / 2;
        output.push_str(newline);
        output.push_str(line_indent);
        output.push_str(&indentation.repeat(depth));
        output.push_str(line.trim_start());
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Parses an edited document the way the daemon does, so every expected
    /// text below is also proven to stay valid JSONC.
    fn parsed(document: &str) -> Value {
        serde_json::from_str(&jsonc_to_json(document).expect("edited JSONC normalizes"))
            .expect("edited JSONC parses")
    }

    fn edited(original: &str, edit: impl FnOnce(&mut String)) -> String {
        let mut document = original.to_string();
        edit(&mut document);
        parsed(&document);
        document
    }

    #[test]
    fn insert_keeps_a_line_comment_with_the_member_it_follows() {
        let original = "{\n  // root\n  \"version\": 1 // v\n}\n";
        let document = edited(original, |document| {
            insert_object_member(document, &[], "modules", &json!({})).unwrap();
        });
        assert_eq!(
            document,
            "{\n  // root\n  \"version\": 1, // v\n  \"modules\": {}\n}\n"
        );
    }

    #[test]
    fn insert_with_decorates_the_rendered_member() {
        let original = "{\r\n  \"modules\": {}\r\n}\r\n";
        let document = edited(original, |document| {
            insert_object_member_with(
                document,
                &["modules"],
                "aft",
                &json!({"program": "/a"}),
                |member, newline| member.insert_str(0, &format!("// added{newline}    ")),
            )
            .unwrap();
        });
        assert_eq!(
            document,
            "{\r\n  \"modules\": {\r\n    // added\r\n    \"aft\": {\r\n      \"program\": \"/a\"\r\n    }\r\n  }\r\n}\r\n"
        );
    }

    /// Fails when any line ends in a space or tab.
    fn assert_no_trailing_whitespace(document: &str) {
        for (number, line) in document.split('\n').enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            assert!(
                !line.ends_with([' ', '\t']),
                "line {} ends in whitespace: {line:?}\n{document:?}",
                number + 1
            );
        }
    }

    #[test]
    fn an_insert_into_a_one_line_object_leaves_no_trailing_whitespace() {
        let original = "{\n  \"modules\": {\n    \"aft\": { \"program\": \"/a\" },\n    \"mc\": { \"program\": \"/m\" /* pinned */ \t},\n    \"empty\": { }\n  }\n}\n";
        let document = edited(original, |document| {
            set_member(document, &["modules", "aft", "enabled"], &json!(false)).unwrap();
            set_member(document, &["modules", "mc", "enabled"], &json!(true)).unwrap();
            set_member(document, &["modules", "empty", "program"], &json!("/e")).unwrap();
        });
        assert_eq!(
            document,
            "{\n  \"modules\": {\n    \"aft\": { \"program\": \"/a\",\n      \"enabled\": false\n    },\n    \"mc\": { \"program\": \"/m\", /* pinned */\n      \"enabled\": true\n    },\n    \"empty\": {\n      \"program\": \"/e\"\n    }\n  }\n}\n"
        );
        assert_no_trailing_whitespace(&document);
    }

    #[test]
    fn inserted_objects_render_their_keys_sorted_in_every_build() {
        // Built in reverse order: with serde_json's `preserve_order` on, the
        // map iterates in this order unless the renderer sorts it.
        let mut value = serde_json::Map::new();
        value.insert("reserved".to_string(), json!(true));
        value.insert("program".to_string(), json!("/c"));
        value.insert("env".to_string(), json!({"Z": "1", "A": "2"}));
        let document = edited("{}", |document| {
            insert_member(document, &["claustrum"], &Value::Object(value)).unwrap();
        });
        assert_eq!(
            document,
            "{\n  \"claustrum\": {\n    \"env\": {\n      \"A\": \"2\",\n      \"Z\": \"1\"\n    },\n    \"program\": \"/c\",\n    \"reserved\": true\n  }\n}"
        );
    }

    #[test]
    fn toggling_enabled_replaces_only_the_value_in_a_crlf_file() {
        let original = "{\r\n  // modules\r\n  \"modules\": {\r\n    \"aft\": {\r\n      \"program\": \"/opt/ck/bin/ck-aft\", // the program\r\n      \"enabled\": true /* on */\r\n    },\r\n  },\r\n}\r\n";
        let document = edited(original, |document| {
            set_member(document, &["modules", "aft", "enabled"], &json!(false)).unwrap();
        });
        assert_eq!(
            document,
            original.replacen("true /* on */", "false /* on */", 1)
        );
    }

    #[test]
    fn toggling_enabled_inserts_the_key_when_absent_and_keeps_trailing_commas() {
        let original = "{\r\n  \"modules\": {\r\n    \"aft\": {\r\n      \"program\": \"/a\", // keep\r\n    },\r\n  },\r\n}\r\n";
        let document = edited(original, |document| {
            set_member(document, &["modules", "aft", "enabled"], &json!(false)).unwrap();
        });
        assert_eq!(
            document,
            "{\r\n  \"modules\": {\r\n    \"aft\": {\r\n      \"program\": \"/a\", // keep\r\n      \"enabled\": false\r\n    },\r\n  },\r\n}\r\n"
        );
    }

    const WITH_ENV: &str = "{\n  \"modules\": {\n    \"aft\": {\n      \"program\": \"/a\",\n      // the environment\n      \"env\": {\n        \"A\": \"1\", // first\n        \"B\": \"2\",\n        \"C\": \"3\"\n      }\n    }\n  }\n}\n";

    #[test]
    fn an_env_member_edit_replaces_only_its_value() {
        let document = edited(WITH_ENV, |document| {
            set_member(document, &["modules", "aft", "env", "B"], &json!("two")).unwrap();
        });
        assert_eq!(
            document,
            WITH_ENV.replacen("\"B\": \"2\"", "\"B\": \"two\"", 1)
        );
    }

    #[test]
    fn an_added_env_member_goes_last_in_its_object() {
        let document = edited(WITH_ENV, |document| {
            set_member(document, &["modules", "aft", "env", "D"], &json!("4")).unwrap();
        });
        assert_eq!(
            document,
            WITH_ENV.replacen(
                "        \"C\": \"3\"\n",
                "        \"C\": \"3\",\n        \"D\": \"4\"\n",
                1
            )
        );
    }

    #[test]
    fn an_added_env_member_creates_the_env_object_when_absent() {
        let original =
            "{\n  \"modules\": {\n    \"aft\": {\n      \"program\": \"/a\"\n    }\n  }\n}\n";
        let document = edited(original, |document| {
            set_member(document, &["modules", "aft", "env", "KEY"], &json!("v")).unwrap();
        });
        assert_eq!(
            document,
            "{\n  \"modules\": {\n    \"aft\": {\n      \"program\": \"/a\",\n      \"env\": {\n        \"KEY\": \"v\"\n      }\n    }\n  }\n}\n"
        );
    }

    #[test]
    fn removing_an_env_member_takes_its_line_and_one_comma() {
        let middle = edited(WITH_ENV, |document| {
            assert!(remove_member(document, &["modules", "aft", "env", "B"]).unwrap());
        });
        assert_eq!(middle, WITH_ENV.replacen("        \"B\": \"2\",\n", "", 1));

        let last = edited(WITH_ENV, |document| {
            assert!(remove_member(document, &["modules", "aft", "env", "C"]).unwrap());
        });
        assert_eq!(last, WITH_ENV.replacen(",\n        \"C\": \"3\"", "", 1));
    }

    #[test]
    fn removing_a_member_keeps_the_comments_outside_its_value() {
        let first = edited(WITH_ENV, |document| {
            assert!(remove_member(document, &["modules", "aft", "env", "A"]).unwrap());
        });
        assert_eq!(
            first,
            WITH_ENV.replacen("\"A\": \"1\", // first", "// first", 1)
        );

        // The whole env object goes, with the comments inside it; the comment
        // above it is outside its value and stays.
        let env = edited(WITH_ENV, |document| {
            assert!(remove_member(document, &["modules", "aft", "env"]).unwrap());
        });
        assert_eq!(
            env,
            "{\n  \"modules\": {\n    \"aft\": {\n      \"program\": \"/a\"\n      // the environment\n    }\n  }\n}\n"
        );
    }

    #[test]
    fn removing_a_member_keeps_a_trailing_comma_and_crlf() {
        let original = "{\r\n  \"a\": 1,\r\n  \"b\": 2,\r\n}\r\n";
        let document = edited(original, |document| {
            assert!(remove_member(document, &["b"]).unwrap());
        });
        assert_eq!(document, "{\r\n  \"a\": 1,\r\n}\r\n");

        let inline = edited(r#"{"a": 1, "b": 2, "c": 3}"#, |document| {
            assert!(remove_member(document, &["b"]).unwrap());
        });
        assert_eq!(inline, r#"{"a": 1, "c": 3}"#);
        let only = edited(r#"{"only": 1,}"#, |document| {
            assert!(remove_member(document, &["only"]).unwrap());
        });
        assert_eq!(only, "{}");
    }

    #[test]
    fn removing_an_absent_member_changes_nothing() {
        let mut document = WITH_ENV.to_string();
        assert!(!remove_member(&mut document, &["modules", "aft", "env", "Z"]).unwrap());
        assert!(!remove_member(&mut document, &["modules", "missing", "env"]).unwrap());
        assert_eq!(document, WITH_ENV);
    }

    const WITH_ARGS: &str = "{\n  \"modules\": {\n    \"nats\": {\n      \"args\": [\"-c\", \"/etc/nats.conf\"], // inline\n      \"list\": [\n        \"first\", // one\n        \"second\" // two\n      ],\n      \"trailing\": [\"a\", \"b\",],\n      \"empty\": []\n    }\n  }\n}\n";
    const ARGS: [&str; 3] = ["modules", "nats", "args"];
    const LIST: [&str; 3] = ["modules", "nats", "list"];

    #[test]
    fn an_argument_edit_replaces_only_that_element() {
        let document = edited(WITH_ARGS, |document| {
            set_element(document, &ARGS, 1, &json!("/srv/nats.conf")).unwrap();
        });
        assert_eq!(
            document,
            WITH_ARGS.replacen("\"/etc/nats.conf\"", "\"/srv/nats.conf\"", 1)
        );
    }

    #[test]
    fn an_added_argument_follows_the_array_s_layout() {
        let inline = edited(WITH_ARGS, |document| {
            push_element(document, &ARGS, &json!("-D")).unwrap();
        });
        assert_eq!(
            inline,
            WITH_ARGS.replacen("\"/etc/nats.conf\"]", "\"/etc/nats.conf\", \"-D\"]", 1)
        );

        let lines = edited(WITH_ARGS, |document| {
            push_element(document, &LIST, &json!("third")).unwrap();
        });
        assert_eq!(
            lines,
            WITH_ARGS.replacen(
                "        \"second\" // two\n",
                "        \"second\", // two\n        \"third\"\n",
                1
            )
        );

        let trailing = edited(WITH_ARGS, |document| {
            push_element(document, &["modules", "nats", "trailing"], &json!("c")).unwrap();
        });
        assert_eq!(
            trailing,
            WITH_ARGS.replacen("[\"a\", \"b\",]", "[\"a\", \"b\", \"c\"]", 1)
        );

        let empty = edited(WITH_ARGS, |document| {
            push_element(document, &["modules", "nats", "empty"], &json!("x")).unwrap();
        });
        assert_eq!(empty, WITH_ARGS.replacen("[]", "[\"x\"]", 1));
    }

    #[test]
    fn the_first_added_argument_creates_the_array() {
        let original = "{\r\n  \"modules\": {\r\n    \"nats\": {\r\n      \"program\": \"/n\"\r\n    }\r\n  }\r\n}\r\n";
        let document = edited(original, |document| {
            push_element(document, &["modules", "nats", "args"], &json!("-c")).unwrap();
        });
        assert_eq!(
            document,
            "{\r\n  \"modules\": {\r\n    \"nats\": {\r\n      \"program\": \"/n\",\r\n      \"args\": [\r\n        \"-c\"\r\n      ]\r\n    }\r\n  }\r\n}\r\n"
        );
    }

    #[test]
    fn removing_an_argument_takes_one_comma_and_keeps_comments() {
        let first = edited(WITH_ARGS, |document| {
            remove_element(document, &ARGS, 0).unwrap();
        });
        assert_eq!(
            first,
            WITH_ARGS.replacen("[\"-c\", \"/etc/nats.conf\"]", "[\"/etc/nats.conf\"]", 1)
        );

        let last = edited(WITH_ARGS, |document| {
            remove_element(document, &ARGS, 1).unwrap();
        });
        assert_eq!(
            last,
            WITH_ARGS.replacen("[\"-c\", \"/etc/nats.conf\"]", "[\"-c\"]", 1)
        );

        let commented_first = edited(WITH_ARGS, |document| {
            remove_element(document, &LIST, 0).unwrap();
        });
        assert_eq!(
            commented_first,
            WITH_ARGS.replacen("\"first\", // one", "// one", 1)
        );

        let commented_last = edited(WITH_ARGS, |document| {
            remove_element(document, &LIST, 1).unwrap();
        });
        assert_eq!(
            commented_last,
            WITH_ARGS.replacen(
                "        \"first\", // one\n        \"second\" // two\n",
                "        \"first\" // one\n        // two\n",
                1
            )
        );

        let trailing = edited(WITH_ARGS, |document| {
            remove_element(document, &["modules", "nats", "trailing"], 1).unwrap();
        });
        assert_eq!(
            trailing,
            WITH_ARGS.replacen("[\"a\", \"b\",]", "[\"a\",]", 1)
        );
    }

    #[test]
    fn edits_refuse_paths_that_do_not_fit_the_document() {
        let mut document = r#"{"modules": {"aft": "not an object", "list": [1]}}"#.to_string();
        let original = document.clone();
        assert_eq!(
            set_member(&mut document, &["modules", "aft", "enabled"], &json!(true)).unwrap_err(),
            "modules.aft is not an object"
        );
        assert_eq!(
            insert_member(&mut document, &["modules", "aft"], &json!({})).unwrap_err(),
            "modules.aft already exists"
        );
        assert_eq!(
            push_element(&mut document, &["modules", "aft"], &json!("x")).unwrap_err(),
            "modules.aft is not an array"
        );
        assert_eq!(
            set_element(&mut document, &["modules", "list"], 1, &json!(2)).unwrap_err(),
            "modules.list has 1 elements, so there is no element 1"
        );
        assert_eq!(
            remove_element(&mut document, &["modules", "absent"], 0).unwrap_err(),
            "modules.absent is absent"
        );
        assert!(set_member(&mut document, &[], &json!(1)).is_err());
        assert_eq!(document, original, "a refused edit writes nothing");
    }

    /// Every comment in the original text, in order.
    fn comments(document: &str) -> Vec<&str> {
        let mut found = Vec::new();
        let bytes = document.as_bytes();
        let mut in_string = false;
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'\\' if in_string => index += 1,
                b'"' => in_string = !in_string,
                b'/' if !in_string && bytes.get(index + 1) == Some(&b'/') => {
                    let end = document[index..]
                        .find('\n')
                        .map_or(document.len(), |end| index + end);
                    found.push(document[index..end].trim_end_matches('\r'));
                    index = end;
                }
                b'/' if !in_string && bytes.get(index + 1) == Some(&b'*') => {
                    let end = index + document[index..].find("*/").expect("closed") + 2;
                    found.push(&document[index..end]);
                    index = end;
                }
                _ => {}
            }
            index += 1;
        }
        found
    }

    #[test]
    fn a_settings_session_keeps_every_comment_outside_the_changed_values() {
        let original = "// operator's settings\r\n{\r\n  \"version\": 1, /* schema */\r\n  \"modules\": {\r\n    // the editor\r\n    \"aft\": {\r\n      \"program\": \"/opt/ck/bin/ck-aft\", // pinned build\r\n      \"args\": [\r\n        \"--serve\", // listen\r\n        \"--quiet\",\r\n      ],\r\n      \"env\": { \"RUST_LOG\": \"info\" /* noisy */ },\r\n    },\r\n    \"mc\": { \"program\": \"/opt/ck/bin/ck-mc\", \"enabled\": false }, // off for now\r\n  },\r\n}\r\n";
        let document = edited(original, |document| {
            set_member(document, &["modules", "mc", "enabled"], &json!(true)).unwrap();
            set_member(document, &["modules", "aft", "enabled"], &json!(false)).unwrap();
            set_element(
                document,
                &["modules", "aft", "args"],
                1,
                &json!("--verbose"),
            )
            .unwrap();
            push_element(document, &["modules", "aft", "args"], &json!("--port=9")).unwrap();
            set_member(
                document,
                &["modules", "aft", "env", "RUST_LOG"],
                &json!("debug"),
            )
            .unwrap();
            set_member(
                document,
                &["modules", "aft", "env", "HOME"],
                &json!("/Users/alex"),
            )
            .unwrap();
            remove_element(document, &["modules", "aft", "args"], 0).unwrap();
        });
        assert_eq!(comments(&document), comments(original));
        assert_no_trailing_whitespace(&document);
        assert!(
            !document.replace("\r\n", "").contains('\n'),
            "every newline stays CRLF:\n{document:?}"
        );
        assert_eq!(
            parsed(&document),
            json!({
                "version": 1,
                "modules": {
                    "aft": {
                        "program": "/opt/ck/bin/ck-aft",
                        "args": ["--verbose", "--port=9"],
                        "env": {"RUST_LOG": "debug", "HOME": "/Users/alex"},
                        "enabled": false,
                    },
                    "mc": {"program": "/opt/ck/bin/ck-mc", "enabled": true},
                },
            })
        );
    }

    #[test]
    fn read_document_returns_the_text_and_its_value() {
        let dir = cortexkit_test_support::ScratchDir::new("subc-jsonc-read");
        let path = dir.join("subc.jsonc");
        let text = "{\n  // note\n  \"version\": 1,\n}\n";
        fs::write(&path, text).unwrap();
        let (read, value) = read_document(&path).unwrap();
        assert_eq!(read, text);
        assert_eq!(value, json!({"version": 1}));

        fs::write(&path, "{\"version\": }").unwrap();
        assert!(read_document(&path)
            .unwrap_err()
            .starts_with(&format!("could not parse {}", path.display())));
        fs::remove_file(&path).unwrap();
        assert!(read_document(&path)
            .unwrap_err()
            .starts_with(&format!("could not read {}", path.display())));
    }
}
