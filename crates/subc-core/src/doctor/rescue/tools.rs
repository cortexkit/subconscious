use std::{
    ffi::OsString,
    fs::{self, File},
    path::{Component, Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;

use super::{
    files::{at, existing_dir, Directory},
    host::{Child, Children, Host, Platform},
    pins::{self, LOCKFILE, MINIMUM_NODE, NODE_VERSION, PI_PACKAGE, PI_VERSION, WRAPPER},
    store::{Lease, Store},
    Context,
};

const PATH_PROBE: &str = r"process.execPath+'\n'+process.version";
const PATH_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

struct Node {
    program: PathBuf,
    npm: Option<PathBuf>,
    lock: Option<File>,
}

impl Node {
    fn private(&self) -> bool {
        self.lock.is_some()
    }
}

pub(super) struct Tools {
    node: Node,
    pi: PathBuf,
    _pi_lock: File,
}

impl Tools {
    pub fn pi_command(
        &self,
        children: &Children,
        cwd: &Path,
        args: impl IntoIterator<Item = OsString>,
    ) -> Result<Child, String> {
        let args = std::iter::once(self.pi.as_os_str().into())
            .chain(args)
            .collect();
        children.command(&self.node.program, args, cwd, self.node.private())
    }
}

pub(super) fn prepare(
    context: &Context<'_, impl Host>,
    folder: &Directory,
) -> Result<Tools, String> {
    let node_store = Store::new(super::managed(folder, "node")?, NODE_VERSION);
    let pi_store = Store::new(super::managed(folder, "pi")?, PI_VERSION);
    let cache = super::managed(folder, "npm-cache")?;
    let logs = super::managed(&cache, "_logs")?;
    let path_node = probe_path_node(context);

    if let Some(lease) = pi_store.get(|root| Ok(pi_script(root)))? {
        let node = select_node(context, &node_store, path_node.as_ref(), false)?;
        let script = pi_store.path().join(&lease.value);
        if pi_version_valid(context, &node, &script)? {
            return Ok(Tools {
                node,
                pi: script,
                _pi_lock: lease.lock,
            });
        }
        // The same Node can serve npm when it already has a CLI script.
        if node.npm.is_some() {
            drop(lease);
            return install_pi(context, &pi_store, node, &cache, &logs);
        }
    }

    let node = select_node(context, &node_store, path_node.as_ref(), true)?;
    install_pi(context, &pi_store, node, &cache, &logs)
}

fn probe_path_node(context: &Context<'_, impl Host>) -> Option<Node> {
    let mut command = context
        .children
        .command(
            Path::new("node"),
            vec!["-p".into(), PATH_PROBE.into()],
            context.tools,
            false,
        )
        .ok()?;
    command.timeout = Some(PATH_PROBE_TIMEOUT);
    let output = context.host.run(command).ok()?;
    if output.code != Some(0) {
        return None;
    }
    let mut lines = std::str::from_utf8(&output.stdout).ok()?.lines();
    let program = PathBuf::from(lines.next()?);
    let version = pins::version(lines.next()?.strip_prefix('v')?)?;
    if lines.next().is_some() || !program.is_absolute() || version < MINIMUM_NODE {
        return None;
    }
    let npm = npm_cli(&program, context.platform()).filter(|path| path.is_file());
    Some(Node {
        program,
        npm,
        lock: None,
    })
}

fn npm_cli(program: &Path, platform: Platform) -> Option<PathBuf> {
    let bin = program.parent()?;
    let base = if platform.windows() {
        bin
    } else {
        &bin.parent()?.join("lib")
    };
    Some(base.join("node_modules/npm/bin/npm-cli.js"))
}

fn node_program(root: &Path, platform: Platform) -> PathBuf {
    root.join(if platform.windows() {
        "node.exe"
    } else {
        "bin/node"
    })
}

fn private_node(context: &Context<'_, impl Host>, root: &Path, need_npm: bool) -> Option<bool> {
    if !existing_dir(root) {
        return None;
    }
    let program = node_program(root, context.platform());
    let root = root.canonicalize().ok()?;
    within_file(&root, &program)?;
    let npm = npm_cli(&program, context.platform())
        .and_then(|path| within_file(&root, &path))
        .is_some();
    if need_npm && !npm {
        return None;
    }
    let check = context
        .children
        .command(&program, vec!["--version".into()], context.tools, true)
        .ok()?;
    let output = context.host.run(check).ok()?;
    let stdout = std::str::from_utf8(&output.stdout).ok()?;
    let version = pins::version(pins::unprefix(stdout.trim()))?;
    (output.code == Some(0) && Some(version) == pins::version(NODE_VERSION)).then_some(npm)
}

fn select_node(
    context: &Context<'_, impl Host>,
    store: &Store,
    path_node: Option<&Node>,
    need_npm: bool,
) -> Result<Node, String> {
    if let Some(node) = path_node.filter(|node| !need_npm || node.npm.is_some()) {
        return Ok(Node {
            program: node.program.clone(),
            npm: node.npm.clone(),
            lock: None,
        });
    }
    let validate = |root: &Path| Ok(private_node(context, root, need_npm));
    let lease = match store.get(validate)? {
        Some(lease) => lease,
        None => store.install(context.host, validate, |staging| {
            install_node(context, staging)
        })?,
    };
    let program = node_program(&store.path(), context.platform());
    let npm = lease
        .value
        .then(|| npm_cli(&program, context.platform()))
        .flatten();
    Ok(Node {
        program,
        npm,
        lock: Some(lease.lock),
    })
}

fn install_node(
    context: &Context<'_, impl Host>,
    staging: &Path,
) -> Result<(PathBuf, bool), String> {
    let platform = context.platform();
    let archive = context.host.archive().ok_or_else(|| {
        format!(
            "no private Node archive for {} {}",
            platform.os, platform.arch
        )
    })?;
    let filename = archive.filename();
    let path = staging.join(&filename);
    let url = format!(
        "{}/v{NODE_VERSION}/{filename}",
        context.host.node_dist().trim_end_matches('/')
    );
    context.host.download(&url, &path)?;
    let digest = crate::setup::upgrade_assets::sha256_file(&path)?;
    if digest != archive.sha256.as_ref() {
        return Err(format!(
            "sha256 mismatch for {filename}: expected {}, got {digest}",
            archive.sha256
        ));
    }
    context.host.extract(&path, staging)?;
    let inner = staging.join(archive.folder());
    let npm = private_node(context, &inner, true).ok_or_else(|| {
        format!("{filename} did not contain Node {NODE_VERSION} with its npm-cli.js")
    })?;
    Ok((inner, npm))
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Bin {
    Script(String),
    Map { pi: String },
}

#[derive(Deserialize)]
struct Manifest {
    bin: Bin,
}

fn within_file(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let file = candidate.canonicalize().ok()?;
    (file.starts_with(root) && file.is_file()).then_some(file)
}

fn pi_script(project: &Path) -> Option<PathBuf> {
    let project = project.canonicalize().ok()?;
    let package = project
        .join("node_modules")
        .join(PI_PACKAGE)
        .canonicalize()
        .ok()?;
    if !package.starts_with(&project) || package == project {
        return None;
    }
    let manifest = within_file(&package, &package.join("package.json"))?;
    let manifest: Manifest = serde_json::from_slice(&fs::read(manifest).ok()?).ok()?;
    let path = match &manifest.bin {
        Bin::Script(path) | Bin::Map { pi: path } => Path::new(path),
    };
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    let script = within_file(&package, &package.join(path))?;
    Some(script.strip_prefix(&project).ok()?.into())
}

fn pi_version_valid(
    context: &Context<'_, impl Host>,
    node: &Node,
    script: &Path,
) -> Result<bool, String> {
    context.check_auth()?;
    let command = context.children.command(
        &node.program,
        vec![script.as_os_str().into(), "--version".into()],
        context.profile.path(),
        node.private(),
    )?;
    Ok(context
        .host
        .run(command)
        .is_ok_and(|output| output.code == Some(0) && pi_version(&output.stdout)))
}

pub(super) fn pi_version(stdout: &[u8]) -> bool {
    std::str::from_utf8(stdout).is_ok_and(|text| {
        text.split_whitespace()
            .any(|token| pins::unprefix(token) == PI_VERSION)
    })
}

fn install_pi(
    context: &Context<'_, impl Host>,
    store: &Store,
    node: Node,
    cache: &Directory,
    logs: &Directory,
) -> Result<Tools, String> {
    let validate = |root: &Path| {
        let Some(script) = pi_script(root) else {
            return Ok(None);
        };
        Ok(pi_version_valid(context, &node, &root.join(&script))?.then_some(script))
    };
    let Lease { value, lock } = store.install(context.host, validate, |staging| {
        let wrapper = staging.join("package.json");
        let lockfile = staging.join("package-lock.json");
        at(&wrapper, fs::write(&wrapper, WRAPPER))?;
        at(&lockfile, fs::write(&lockfile, LOCKFILE))?;
        let npm = node.npm.as_deref().ok_or("Node does not have npm-cli.js")?;
        let args = std::iter::once(npm.as_os_str().into())
            .chain(["ci", "--ignore-scripts", "--no-audit", "--no-fund"].map(OsString::from))
            .collect();
        let mut command = context
            .children
            .command(&node.program, args, staging, node.private())?;
        for (name, value) in [
            ("npm_config_cache", cache.path().as_os_str()),
            ("npm_config_logs_dir", logs.path().as_os_str()),
            ("npm_config_update_notifier", "false".as_ref()),
        ] {
            command
                .env
                .retain(|(key, _)| !key.eq_ignore_ascii_case(name));
            command.env.push((name.into(), value.into()));
        }
        let output = context
            .host
            .run(command)
            .map_err(|error| format!("npm ci: {error}"))?;
        if output.code != Some(0) {
            return Err(format!("npm ci: {}", output.description()));
        }
        let script = validate(staging)?
            .ok_or_else(|| format!("staged Pi did not report version {PI_VERSION}"))?;
        Ok((staging.into(), script))
    })?;
    Ok(Tools {
        node,
        pi: store.path().join(value),
        _pi_lock: lock,
    })
}
