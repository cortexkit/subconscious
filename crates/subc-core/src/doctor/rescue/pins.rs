use std::borrow::Cow;

use super::host::Platform;

pub(super) const PI_PACKAGE: &str = "@earendil-works/pi-coding-agent";
pub(super) const PI_VERSION: &str = "1.6.0";
pub(super) const WRAPPER: &[u8] = include_bytes!("pins/package.json");
pub(super) const LOCKFILE: &[u8] = include_bytes!("pins/package-lock.json");
pub(super) const NODE_VERSION: &str = "26.13.1";
pub(super) const NODE_DIST: &str = "https://nodejs.org/dist/";
pub(super) const MINIMUM_NODE: (u64, u64, u64) = (22, 19, 0);

pub(super) struct NodeArchive {
    pub target: &'static str,
    pub extension: &'static str,
    pub sha256: Cow<'static, str>,
}

impl NodeArchive {
    pub fn folder(&self) -> String {
        format!("node-v{NODE_VERSION}-{}", self.target)
    }

    pub fn filename(&self) -> String {
        format!("{}.{}", self.folder(), self.extension)
    }
}

pub(super) fn node_archive(platform: Platform) -> Option<NodeArchive> {
    let (target, extension, sha256) = match (platform.os, platform.arch) {
        ("macos", "aarch64") => (
            "darwin-arm64",
            "tar.gz",
            "aec2514ea1ceb2ba52455e9ec65986f6749a06dd759e5db2a72e6889668c9735",
        ),
        ("macos", "x86_64") => (
            "darwin-x64",
            "tar.gz",
            "82bb96078a24ca50b9c9a38f2379e6be6d87171fcd400fc14c8934b3bce96e2f",
        ),
        ("linux", "x86_64") => (
            "linux-x64",
            "tar.gz",
            "518674de2b621728a5e4486f5fb3b7d2900bb3cc4aa97969edaa463db99f9778",
        ),
        ("linux", "aarch64") => (
            "linux-arm64",
            "tar.gz",
            "80a330b35415382878df18d41afbd106ea89bfb3be0087d6608e289a14c5cc7d",
        ),
        ("windows", "x86_64") => (
            "win-x64",
            "zip",
            "e2edda75bfb089654705cf65a13b58933bfd5f57148e8ebda916f58a2474f1f7",
        ),
        ("windows", "aarch64") => (
            "win-arm64",
            "zip",
            "181ebe431b90787113a9e4fc80d38ce86d37888a353a0386de44d2872e48cb97",
        ),
        _ => return None,
    };
    Some(NodeArchive {
        target,
        extension,
        sha256: sha256.into(),
    })
}

pub(super) type Version = (u64, u64, u64);

pub(super) fn version(value: &str) -> Option<Version> {
    fn part(value: &str) -> Option<u64> {
        (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| value.parse().ok())
            .flatten()
    }
    let mut parts = value.split('.');
    let version = (
        part(parts.next()?)?,
        part(parts.next()?)?,
        part(parts.next()?)?,
    );
    parts.next().is_none().then_some(version)
}

pub(super) fn unprefix(token: &str) -> &str {
    token.strip_prefix('v').unwrap_or(token)
}
