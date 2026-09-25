use serde_json::Value;
use std::{fs, path::Path, process::Command};

fn metadata() -> Value {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
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

#[test]
fn leaf_crate_is_unpublished_and_has_no_workspace_dependencies() {
    let manifest = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("read leaf manifest");
    assert!(
        manifest
            .lines()
            .any(|line| line.trim() == "publish = false"),
        "leaf crate must be publish = false"
    );
    let data = metadata();
    let package = data["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["name"] == "subc-test-support")
        .unwrap();
    assert_eq!(
        package["publish"],
        serde_json::json!([]),
        "leaf crate must not be publishable"
    );
    for dependency in package["dependencies"].as_array().unwrap() {
        assert!(
            dependency["path"].is_null(),
            "leaf crate has a workspace path dependency: {}",
            dependency["name"]
        );
    }
    for package in data["packages"].as_array().unwrap() {
        for dependency in package["dependencies"].as_array().unwrap() {
            if dependency["name"] == "subc-test-support" {
                assert_eq!(
                    dependency["kind"], "dev",
                    "{} must use the guard only as a dev-dependency",
                    package["name"]
                );
            }
        }
    }
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
        if package["name"] == "subc-test-support" {
            continue;
        }
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
        "test temp directory creation outside subc-test-support:\n{}",
        offenders.join("\n")
    );
}

fn scan_tree(dir: &Path, tests: bool, offenders: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            scan_tree(&path, tests, offenders);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = fs::read_to_string(&path).unwrap();
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
                    body.contains("temp_dir()")
                        .then(|| body[3..].split('(').next().unwrap_or("").trim())
                })
                .collect();
            for (index, (start, _)) in functions.iter().enumerate() {
                let end = functions.get(index + 1).map_or(scope.len(), |next| next.0);
                let body = &scope[*start..end];
                if !(body.contains("create_dir") || body.contains("mkdir")) {
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
