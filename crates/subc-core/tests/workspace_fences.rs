use serde_json::Value;
use std::{fs, path::Path, process::Command};

fn metadata() -> Value {
    let output = Command::new("cargo")
        .args(["metadata", "--locked", "--no-deps", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("metadata JSON")
}

/// Crates that exist only to be test dependencies of other crates. They may take
/// the shared test helper as a normal dependency because they are never linked
/// into a production binary; the second check below keeps it that way.
const TEST_ONLY_CRATES: &[&str] = &["cortexkit-bus-testkit"];

#[test]
fn shared_test_support_is_a_registry_dev_dependency() {
    let data = metadata();
    let mut consumers = 0;
    for package in data["packages"].as_array().unwrap() {
        assert_ne!(
            package["name"], "subc-test-support",
            "local helper must stay retired"
        );
        let test_only = TEST_ONLY_CRATES.contains(&package["name"].as_str().unwrap());
        for dependency in package["dependencies"].as_array().unwrap() {
            if TEST_ONLY_CRATES.contains(&dependency["name"].as_str().unwrap()) {
                assert_eq!(
                    dependency["kind"], "dev",
                    "{} must take the test-only crate {} only as a dev-dependency",
                    package["name"], dependency["name"]
                );
            }
            if dependency["name"] == "cortexkit-test-support" {
                consumers += 1;
                if !test_only {
                    assert_eq!(
                        dependency["kind"], "dev",
                        "{} must use the guard only as a dev-dependency",
                        package["name"]
                    );
                }
                assert!(
                    dependency["path"].is_null()
                        && dependency["source"]
                            .as_str()
                            .is_some_and(|source| source.starts_with("registry+")),
                    "{} must use the published shared helper, not a local copy",
                    package["name"]
                );
            }
        }
    }
    assert!(consumers > 0, "workspace must use the shared helper");
}

// Production connection-file fallbacks in subc-daemon/bootstrap.rs and
// subc-mcp/main.rs create files, not test directories. Production temporary
// workspaces in subc-core/setup/{components,upgrade_assets}.rs are likewise
// outside this test-source scan.
#[test]
fn test_sources_do_not_create_directories_from_temp_dir() {
    let data = metadata();
    let mut offenders = Vec::new();
    for package in data["packages"].as_array().unwrap() {
        let manifest = Path::new(package["manifest_path"].as_str().unwrap());
        let crate_dir = manifest.parent().unwrap();
        for folder in ["src", "tests"] {
            let root = crate_dir.join(folder);
            if root.exists() {
                scan_tree(&root, folder == "tests", &mut offenders);
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "test temp directory creation outside cortexkit-test-support:\n{}",
        offenders.join("\n")
    );
}

/// The one test function allowed to create a directory straight under
/// `temp_dir()`: the daemon kit's data home holds the daemon's Unix socket,
/// whose path is limited to about a hundred bytes, and every directory under
/// the shared scratch root is too long for it. The kit removes the directory
/// when it drops. Keyed by file and function name, so nothing else in the file
/// or a new function of the same name elsewhere gains the exemption.
const SHORT_SOCKET_ROOT: (&str, &str) = ("tests/common/kit.rs", "create_short_socket_root");

fn is_short_socket_root(path: &Path, function: &str) -> bool {
    path.ends_with(SHORT_SOCKET_ROOT.0) && function == SHORT_SOCKET_ROOT.1
}

fn scan_tree(dir: &Path, tests: bool, offenders: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        // The scanner's own string patterns mention forbidden creation without creating scratch.
        if path == Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/workspace_fences.rs") {
            continue;
        }
        if path.is_dir() {
            scan_tree(&path, tests, offenders);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            // Normalize line endings first: a Windows checkout has CRLF, and the
            // `#[cfg(test)]\n` markers below would otherwise miss there and pick a
            // different (earlier) scope than on Unix.
            let source = fs::read_to_string(&path).unwrap().replace("\r\n", "\n");
            let scope = if tests {
                source.as_str()
            } else if let Some(pos) = source
                .find("#[cfg(test)]\nmod ")
                .or_else(|| source.find("#[cfg(test)]\nfn "))
                .or_else(|| source.find("#[cfg(all(test,"))
            {
                &source[pos..]
            } else {
                continue;
            };
            // Names of path factories whose result is rooted at temp_dir(). A
            // creator may call one rather than mentioning temp_dir() itself.
            let functions: Vec<_> = scope.match_indices("fn ").collect();
            let factories: Vec<_> = functions
                .iter()
                .enumerate()
                .filter_map(|(index, (start, _))| {
                    let end = functions.get(index + 1).map_or(scope.len(), |next| next.0);
                    let body = &scope[*start..end];
                    let name = body[3..].split('(').next().unwrap_or("").trim();
                    (body.contains("temp_dir()") && !is_short_socket_root(&path, name))
                        .then_some(name)
                })
                .collect();
            for (index, (start, _)) in functions.iter().enumerate() {
                let end = functions.get(index + 1).map_or(scope.len(), |next| next.0);
                let body = &scope[*start..end];
                if !(body.contains("create_dir") || body.contains("mkdir")) {
                    continue;
                }
                if is_short_socket_root(&path, body[3..].split('(').next().unwrap_or("").trim()) {
                    continue;
                }
                if body.contains("temp_dir()")
                    || factories
                        .iter()
                        .any(|name| !name.is_empty() && body.contains(&format!("{name}(")))
                {
                    let offset = body
                        .find("create_dir")
                        .or_else(|| body.find("mkdir"))
                        .unwrap();
                    let original_line = source[..source.len() - scope.len() + start + offset]
                        .bytes()
                        .filter(|byte| *byte == b'\n')
                        .count()
                        + 1;
                    offenders.push(format!("{}:{original_line}", path.display()));
                }
            }
        }
    }
}
