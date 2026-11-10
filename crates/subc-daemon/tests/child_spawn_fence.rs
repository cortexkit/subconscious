//! Keep every production fork behind the socketpair creation lock. Test-only
//! items and their out-of-line modules are excluded, independent of host cfgs.
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
struct Token {
    text: String,
    line: usize,
}

fn tokens(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let (mut i, mut line) = (0, 1);
    let mut result = Vec::new();
    while i < bytes.len() {
        let start = i;
        if bytes[i..].starts_with(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    line += usize::from(bytes[i] == b'\n');
                    i += 1;
                }
            }
            continue;
        }
        if bytes[i].is_ascii_whitespace() {
            line += usize::from(bytes[i] == b'\n');
            i += 1;
            continue;
        }
        let token_line = line;
        let hashes = if bytes[i] == b'r' {
            bytes[i + 1..].iter().take_while(|b| **b == b'#').count()
        } else {
            0
        };
        if bytes[i] == b'r' && bytes.get(i + 1 + hashes) == Some(&b'"') {
            i += 2 + hashes;
            while i < bytes.len() {
                if bytes[i] == b'"'
                    && bytes
                        .get(i + 1..i + 1 + hashes)
                        .is_some_and(|v| v.iter().all(|b| *b == b'#'))
                {
                    i += 1 + hashes;
                    break;
                }
                line += usize::from(bytes[i] == b'\n');
                i += 1;
            }
            result.push(Token {
                text: "literal".into(),
                line: token_line,
            });
            continue;
        }
        if bytes[i] == b'"'
            || (bytes[i] == b'\''
                && (bytes.get(i + 2) == Some(&b'\'') || bytes.get(i + 1) == Some(&b'\\')))
        {
            let quote = bytes[i];
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if bytes[i] == quote {
                    i += 1;
                    break;
                }
                line += usize::from(bytes[i] == b'\n');
                i += 1;
            }
        } else if bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
        } else {
            i += source[i..].chars().next().unwrap().len_utf8();
        }
        result.push(Token {
            text: source[start..i].into(),
            line: token_line,
        });
    }
    result
}

fn matching(tokens: &[Token], start: usize, open: &str, close: &str) -> usize {
    let mut depth = 0;
    for (i, token) in tokens.iter().enumerate().skip(start) {
        if token.text == open {
            depth += 1;
        }
        if token.text == close {
            depth -= 1;
            if depth == 0 {
                return i;
            }
        }
    }
    panic!("unclosed {open}");
}

// Evaluate only the test predicate. Unknown target/feature predicates remain
// possibly enabled, so cfg(any(test, feature = "...")) cannot hide a fork.
fn production_cfg(ts: &[&str], position: &mut usize) -> Option<bool> {
    let name = ts[*position];
    *position += 1;
    if matches!(name, "all" | "any" | "not") {
        assert_eq!(ts[*position], "(");
        *position += 1;
        let mut values = Vec::new();
        while ts[*position] != ")" {
            values.push(production_cfg(ts, position));
            if ts[*position] == "," {
                *position += 1;
            }
        }
        *position += 1;
        match name {
            "not" => values[0].map(|v| !v),
            "all" if values.contains(&Some(false)) => Some(false),
            "all" if values.iter().all(|v| *v == Some(true)) => Some(true),
            "any" if values.contains(&Some(true)) => Some(true),
            "any" if values.iter().all(|v| *v == Some(false)) => Some(false),
            _ => None,
        }
    } else if name == "test" {
        Some(false)
    } else {
        if ts.get(*position) == Some(&"=") {
            *position += 2;
        }
        None
    }
}

fn production_tokens(path: &Path, source: &str, excluded: &mut BTreeSet<PathBuf>) -> Vec<Token> {
    let ts = tokens(source);
    let mut output = Vec::new();
    let mut i = 0;
    while i < ts.len() {
        if ts[i].text != "#" || ts.get(i + 1).is_none_or(|t| t.text != "[") {
            output.push(ts[i].clone());
            i += 1;
            continue;
        }
        let attr_start = i;
        let mut test_only = false;
        let mut module_path = None;
        while ts.get(i).is_some_and(|t| t.text == "#")
            && ts.get(i + 1).is_some_and(|t| t.text == "[")
        {
            let end = matching(&ts, i + 1, "[", "]");
            let attr: Vec<_> = ts[i + 2..end].iter().map(|t| t.text.as_str()).collect();
            test_only |= attr == ["test"]
                || (attr.first() == Some(&"cfg") && production_cfg(&attr, &mut 2) == Some(false));
            if attr.first() == Some(&"path") {
                module_path = attr.last().map(|s| s.trim_matches('"').to_owned());
            }
            i = end + 1;
        }
        if !test_only {
            output.extend_from_slice(&ts[attr_start..i]);
            continue;
        }
        let start = i;
        while i < ts.len() && ts[i].text != ";" && ts[i].text != "{" {
            i += 1;
        }
        if ts.get(i).is_some_and(|t| t.text == ";") {
            if let Some(pos) = ts[start..i].iter().position(|t| t.text == "mod") {
                let name = &ts[start + pos + 1].text;
                let parent = path.parent().unwrap();
                let folder = if matches!(
                    path.file_stem().unwrap().to_str(),
                    Some("lib" | "main" | "mod")
                ) {
                    parent.to_path_buf()
                } else {
                    parent.join(path.file_stem().unwrap())
                };
                if let Some(relative) = module_path {
                    excluded.insert(parent.join(relative));
                } else {
                    excluded.insert(folder.join(format!("{name}.rs")));
                    excluded.insert(folder.join(name));
                }
            }
            i += 1;
        } else if i < ts.len() {
            let end = matching(&ts, i, "{", "}");
            for window in ts[i..end].windows(4) {
                if window[0].text == "include" && window[1].text == "!" && window[2].text == "(" {
                    excluded.insert(
                        path.parent()
                            .unwrap()
                            .join(window[3].text.trim_matches('"')),
                    );
                }
            }
            i = end + 1;
        }
    }
    output
}

fn sources(path: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, files);
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
}

fn module_files(path: &Path, ts: &[Token]) -> Vec<PathBuf> {
    let parent = path.parent().unwrap();
    let folder = if matches!(
        path.file_stem().unwrap().to_str(),
        Some("lib" | "main" | "mod")
    ) {
        parent.to_path_buf()
    } else {
        parent.join(path.file_stem().unwrap())
    };
    let mut files = Vec::new();
    for i in 0..ts.len().saturating_sub(2) {
        if ts[i].text != "mod" || ts[i + 2].text != ";" {
            continue;
        }
        let start = ts[..i]
            .iter()
            .rposition(|t| matches!(t.text.as_str(), ";" | "{" | "}"))
            .map_or(0, |p| p + 1);
        let explicit = ts[start..i].windows(3).find_map(|w| {
            (w[0].text == "path" && w[1].text == "=").then(|| w[2].text.trim_matches('"'))
        });
        let candidates = if let Some(relative) = explicit {
            vec![parent.join(relative), folder.join(relative)]
        } else {
            vec![
                folder.join(format!("{}.rs", ts[i + 1].text)),
                folder.join(&ts[i + 1].text).join("mod.rs"),
            ]
        };
        files.extend(candidates.into_iter().filter_map(|p| p.canonicalize().ok()));
    }
    files
}

#[test]
fn production_child_spawns_use_the_shared_fork_boundary() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut files = Vec::new();
    for name in ["subc-daemon", "subc-os"] {
        sources(&workspace.join(name).join("src"), &mut files);
    }
    let mut excluded = BTreeSet::new();
    let parsed: Vec<_> = files
        .iter()
        .map(|path| {
            (
                path,
                production_tokens(path, &fs::read_to_string(path).unwrap(), &mut excluded),
            )
        })
        .collect();
    excluded = excluded
        .into_iter()
        .filter_map(|p| p.canonicalize().ok())
        .collect();
    loop {
        let before = excluded.len();
        for path in &files {
            if excluded.contains(&path.canonicalize().unwrap()) {
                excluded.extend(module_files(
                    path,
                    &tokens(&fs::read_to_string(path).unwrap()),
                ));
            }
        }
        if excluded.len() == before {
            break;
        }
    }
    let mut violations = Vec::new();
    for (path, tokens) in parsed {
        // Only this module may call Command directly: it holds the shared lock
        // across spawn, and releases it before waiting for the running child.
        if path == &workspace.join("subc-os/src/child_spawn.rs") {
            continue;
        }
        if excluded.contains(&path.canonicalize().unwrap()) {
            continue;
        }
        let mut command_names = BTreeSet::new();
        for (index, token) in tokens.iter().enumerate() {
            if token.text != "let" {
                continue;
            }
            let binding =
                index + 1 + usize::from(tokens.get(index + 1).is_some_and(|t| t.text == "mut"));
            let end = tokens[index..]
                .iter()
                .position(|t| t.text == ";")
                .map_or(tokens.len(), |p| index + p);
            if tokens[index..end].iter().any(|t| t.text == "Command") {
                command_names.insert(tokens[binding].text.clone());
            }
        }
        for (index, window) in tokens.windows(4).enumerate() {
            if window[0].text == "."
                && matches!(window[1].text.as_str(), "spawn" | "output" | "status")
                && window[2].text == "("
                && window[3].text == ")"
            {
                // status() is also a common domain getter. Unlike spawn/output,
                // reject it only on a Command constructor or Command binding.
                if window[1].text == "status" {
                    let start = tokens[..index]
                        .iter()
                        .rposition(|t| matches!(t.text.as_str(), ";" | "{" | "}" | "="))
                        .map_or(0, |p| p + 1);
                    if !tokens[start..index]
                        .iter()
                        .any(|t| t.text == "Command" || command_names.contains(&t.text))
                    {
                        continue;
                    }
                }
                violations.push(format!(
                    "{}:{} bare .{}()",
                    path.display(),
                    window[0].line,
                    window[1].text
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "every production process creation must use subc_os::child_spawn:\n{}",
        violations.join("\n")
    );
    println!("scanned {} Rust source files", files.len());
}
