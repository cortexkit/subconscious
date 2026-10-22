//! Exact bytes setup writes into `subc.jsonc` for its fixtures.
//!
//! The other configuration tests mostly compare parsed values, so a change to
//! how an inserted member is laid out (its indentation, separators, newline
//! style or the claustrum comment) would pass them unnoticed. These pins fail
//! on any byte difference instead, after the program paths described below
//! are replaced by tokens.
//!
//! The fixtures that plan a component use the binary home `/opt/ck/bin`; the
//! bus health fixture names its own fixed paths. Joining a program name to
//! it gives a different path on Windows (`\`, `.exe`), so before comparing, each
//! JSON-encoded program path in the written text is replaced by the token
//! `@<program>@`, for example `@ck-mc@`. Everything else is compared as is.
//!
//! The same bytes are expected whether or not serde_json's `preserve_order`
//! feature is on in the build: setup renders every object it writes with its
//! keys sorted, the order a release build of `ck` has always written.

use std::path::Path;

use serde_json::json;

use super::*;
use cortexkit_test_support::ScratchDir;

const BINARY_HOME: &str = "/opt/ck/bin";
const KEY_PATH: &str = "/opt/ck/keys/master.key";
const PROGRAMS: [&str; 5] = ["ck-aft", "ck-mc", "ck-insula", "ck-claustrum", "ck-synapse"];

/// Replaces each JSON-encoded program path under the binary home with its
/// `@<program>@` token, so one expected text holds on every platform.
fn tokenize_programs(written: &str) -> String {
    let mut normalized = written.to_string();
    for program in PROGRAMS {
        let encoded = serde_json::to_string(
            &*Path::new(BINARY_HOME)
                .join(platform_binary(program))
                .to_string_lossy(),
        )
        .expect("a path encodes as a JSON string");
        normalized = normalized.replace(&encoded, &format!("@{program}@"));
    }
    normalized
}

/// Plans `component` over `original` (or over no file at all) and returns the
/// bytes setup would write, with program paths tokenized.
fn planned(name: &str, original: Option<&str>, component: Component, key: Option<&str>) -> String {
    let root = ScratchDir::new(name);
    let config = root.join("subc.jsonc");
    if let Some(original) = original {
        fs::write(&config, original).expect("write fixture");
    }
    let change = plan_component_with_key(
        &config,
        component,
        Path::new(BINARY_HOME),
        key.map(Path::new),
    )
    .expect("setup's change is additive")
    .expect("the fixture lacks the component");
    tokenize_programs(&change.after)
}

/// Asserts the bytes and prints them escaped on a mismatch, so a deliberate
/// layout change can be reviewed line by line.
fn assert_pinned(name: &str, actual: &str, expected: &str) {
    assert!(
        actual == expected,
        "{name}: setup wrote different bytes\n--- expected\n{expected:?}\n--- actual\n{actual:?}"
    );
}

const COMMENTED_VERSION: &str = "{\n  // operator's version must survive\n  \"version\": 1,\n  \"modules\": {\n    // existing module\n    \"custom\": {\"program\": \"/usr/bin/custom\"}\n  }\n}\n";
const NOTE_AND_EMPTY_MODULES: &str =
    "{\n  // keep this note\n  \"version\": 1,\n  \"modules\": {}\n}\n";
const VERSION_ONLY: &str = "{\n  \"version\": 1\n}\n";
const FOUR_SPACE: &str = "{\n    \"version\": 1\n}\n";
const COMPACT_AFT: &str =
    r#"{"version":1,"modules":{"aft":{"program":"/user/aft","reserved":true}}}"#;
const COMMENTED_MODULES: &str = "{\n  // root comment\n  \"version\": 1, // version comment\n  /* preserve this block comment */\n  \"modules\": {\n    // keep the module comment\n    \"aft\": { \"program\": \"/user/aft\" }, // keep the aft comment\n  }, // keep the modules comment\n}\n";
const EMPTY_MODULES: &str = "{\n  \"version\": 1,\n  \"modules\": {}\n}\n";
const CRLF_COMMENTED: &str =
    "{\r\n  // a CRLF file\r\n  \"version\": 1,\r\n  \"modules\": {\r\n    \"aft\": { \"program\": \"/user/aft\" } // aft\r\n  }\r\n}\r\n";

/// A hand-placed bus, as the bus-monitoring tests write it, with fixed paths.
const BUS_INSTALL: &str = "{\n  // the operator's comment survives\n  \"version\": 1,\n  \"modules\": {\n    \"ckbus\": { \"program\": \"/opt/bus/ck-bus\" },\n    \"nats-server\": {\n      \"program\": \"/opt/homebrew/bin/nats-server\",\n      \"args\": [\"-c\", \"/opt/bus/nats/server.conf\"],\n      \"protocol\": \"none\"\n    }\n  }\n}\n";

/// Every fixture's written bytes, by fixture name.
fn written_by_fixture() -> Vec<(&'static str, String)> {
    let mut written = vec![
        (
            "core over a commented version",
            planned(
                "pin-core-commented",
                Some(COMMENTED_VERSION),
                Component::Core,
                None,
            ),
        ),
        (
            "core over a note and empty modules",
            planned(
                "pin-core-note",
                Some(NOTE_AND_EMPTY_MODULES),
                Component::Core,
                None,
            ),
        ),
        (
            "core over a version only",
            planned(
                "pin-core-version",
                Some(VERSION_ONLY),
                Component::Core,
                None,
            ),
        ),
        (
            "core over four-space indentation",
            planned("pin-core-four", Some(FOUR_SPACE), Component::Core, None),
        ),
        (
            "core over a CRLF file",
            planned("pin-core-crlf", Some(CRLF_COMMENTED), Component::Core, None),
        ),
        (
            "mc over compact JSON",
            planned("pin-mc-compact", Some(COMPACT_AFT), Component::Mc, None),
        ),
        (
            "mc over commented modules with trailing commas",
            planned(
                "pin-mc-commented",
                Some(COMMENTED_MODULES),
                Component::Mc,
                None,
            ),
        ),
        (
            "mc over empty modules",
            planned("pin-mc-empty", Some(EMPTY_MODULES), Component::Mc, None),
        ),
        (
            "mc over a CRLF file",
            planned("pin-mc-crlf", Some(CRLF_COMMENTED), Component::Mc, None),
        ),
        (
            "claustrum with a key over a CRLF file",
            planned(
                "pin-claustrum-crlf",
                Some(CRLF_COMMENTED),
                Component::Claustrum,
                Some(KEY_PATH),
            ),
        ),
    ];
    for (name, component, key) in [
        ("fresh core", Component::Core, None),
        ("fresh aft", Component::Aft, None),
        ("fresh mc", Component::Mc, None),
        ("fresh insula", Component::Insula, None),
        ("fresh claustrum", Component::Claustrum, None),
        (
            "fresh claustrum with a key",
            Component::Claustrum,
            Some(KEY_PATH),
        ),
        ("fresh synapse", Component::Synapse, None),
    ] {
        written.push((name, planned("pin-fresh", None, component, key)));
    }

    // Claustrum's entry inserted into a file setup wrote for aft carries the
    // comment explaining `reserved` inside the inserted member.
    for (name, key) in [
        ("claustrum over a written aft file", None),
        (
            "claustrum with a key over a written aft file",
            Some(KEY_PATH),
        ),
    ] {
        let root = ScratchDir::new("pin-claustrum-over-aft");
        let config = root.join("subc.jsonc");
        let aft = plan_component(&config, Component::Aft, Path::new(BINARY_HOME))
            .expect("aft is additive")
            .expect("aft is absent");
        apply(&aft).expect("write aft");
        let claustrum = plan_component_with_key(
            &config,
            Component::Claustrum,
            Path::new(BINARY_HOME),
            key.map(Path::new),
        )
        .expect("claustrum is additive")
        .expect("claustrum is absent");
        written.push((name, tokenize_programs(&claustrum.after)));
    }

    let root = ScratchDir::new("pin-bus-health");
    let config = root.join("subc.jsonc");
    fs::write(&config, BUS_INSTALL).expect("write fixture");
    let health = plan_missing_value(
        &config,
        "modules.nats-server.health",
        json!({
            "http": "http://127.0.0.1:18222/healthz",
            "cadence_ms": 30000,
            "deadline_ms": 5000,
        }),
    )
    .expect("health is additive")
    .expect("health is absent");
    written.push(("bus health check over a hand-placed bus", health.after));

    // Rollback after the operator edited the file removes only setup's values.
    let root = ScratchDir::new("pin-rollback-note");
    let config = root.join("subc.jsonc");
    fs::write(&config, NOTE_AND_EMPTY_MODULES).expect("write fixture");
    let change = plan_component(&config, Component::Core, Path::new(BINARY_HOME))
        .expect("core is additive")
        .expect("storage is absent");
    apply(&change).expect("write core");
    let current = fs::read_to_string(&config).expect("read");
    fs::write(
        &config,
        current.replace("// keep this note", "// edited by operator"),
    )
    .expect("operator edit");
    assert!(rollback_change(&change).expect("rollback"));
    written.push((
        "rollback after an edited comment",
        fs::read_to_string(&config).expect("read"),
    ));

    let root = ScratchDir::new("pin-rollback-sibling");
    let config = root.join("subc.jsonc");
    fs::write(&config, VERSION_ONLY).expect("write fixture");
    let change = plan_component(&config, Component::Core, Path::new(BINARY_HOME))
        .expect("core is additive")
        .expect("storage is absent");
    apply(&change).expect("write core");
    let current = fs::read_to_string(&config).expect("read");
    fs::write(
        &config,
        current.replace(
            "\"backend\": \"sqlite\"",
            "\"backend\": \"sqlite\", \"data_home\": \"/custom\" // operator setting",
        ),
    )
    .expect("operator edit");
    assert!(rollback_change(&change).expect("rollback"));
    written.push((
        "rollback beside an operator's sibling",
        fs::read_to_string(&config).expect("read"),
    ));
    written
}

/// One fixture's expected bytes.
struct Pin {
    fixture: &'static str,
    bytes: &'static str,
}

#[test]
fn setup_writes_the_pinned_bytes_for_every_fixture() {
    let written = written_by_fixture();
    let mut names = written.iter().map(|(name, _)| *name).collect::<Vec<_>>();
    let mut pinned = PINS.iter().map(|pin| pin.fixture).collect::<Vec<_>>();
    names.sort_unstable();
    pinned.sort_unstable();
    assert_eq!(names, pinned, "every fixture has exactly one pin");
    for (name, actual) in &written {
        let pin = PINS
            .iter()
            .find(|pin| pin.fixture == *name)
            .expect("a pin for the fixture");
        assert_pinned(name, actual, pin.bytes);
    }
}

const PINS: &[Pin] = &[
    Pin {
        fixture: "core over a commented version",
        bytes: concat!(
            "{\n",
            "  // operator's version must survive\n",
            "  \"version\": 1,\n",
            "  \"modules\": {\n",
            "    // existing module\n",
            "    \"custom\": {\"program\": \"/usr/bin/custom\"}\n",
            "  },\n",
            "  \"storage\": {\n",
            "    \"backend\": \"sqlite\"\n",
            "  }\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "core over a note and empty modules",
        bytes: concat!(
            "{\n",
            "  // keep this note\n",
            "  \"version\": 1,\n",
            "  \"modules\": {},\n",
            "  \"storage\": {\n",
            "    \"backend\": \"sqlite\"\n",
            "  }\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "core over a version only",
        bytes: concat!(
            "{\n",
            "  \"version\": 1,\n",
            "  \"storage\": {\n",
            "    \"backend\": \"sqlite\"\n",
            "  }\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "core over four-space indentation",
        bytes: concat!(
            "{\n",
            "    \"version\": 1,\n",
            "    \"storage\": {\n",
            "        \"backend\": \"sqlite\"\n",
            "    }\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "core over a CRLF file",
        bytes: concat!(
            "{\r\n",
            "  // a CRLF file\r\n",
            "  \"version\": 1,\r\n",
            "  \"modules\": {\r\n",
            "    \"aft\": { \"program\": \"/user/aft\" } // aft\r\n",
            "  },\r\n",
            "  \"storage\": {\r\n",
            "    \"backend\": \"sqlite\"\r\n",
            "  }\r\n",
            "}\r\n",
        ),
    },
    Pin {
        fixture: "mc over compact JSON",
        bytes: concat!(
            "{\"version\":1,\"modules\":{\"aft\":{\"program\":\"/user/aft\",\"reserved\":true},\n",
            "    \"magic-context\": {\n",
            "      \"program\": @ck-mc@\n",
            "    }\n",
            "  }}",
        ),
    },
    Pin {
        fixture: "mc over commented modules with trailing commas",
        bytes: concat!(
            "{\n",
            "  // root comment\n",
            "  \"version\": 1, // version comment\n",
            "  /* preserve this block comment */\n",
            "  \"modules\": {\n",
            "    // keep the module comment\n",
            "    \"aft\": { \"program\": \"/user/aft\" }, // keep the aft comment\n",
            "    \"magic-context\": {\n",
            "      \"program\": @ck-mc@\n",
            "    }\n",
            "  }, // keep the modules comment\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "mc over empty modules",
        bytes: concat!(
            "{\n",
            "  \"version\": 1,\n",
            "  \"modules\": {\n",
            "    \"magic-context\": {\n",
            "      \"program\": @ck-mc@\n",
            "    }\n",
            "  }\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "mc over a CRLF file",
        bytes: concat!(
            "{\r\n",
            "  // a CRLF file\r\n",
            "  \"version\": 1,\r\n",
            "  \"modules\": {\r\n",
            "    \"aft\": { \"program\": \"/user/aft\" }, // aft\r\n",
            "    \"magic-context\": {\r\n",
            "      \"program\": @ck-mc@\r\n",
            "    }\r\n",
            "  }\r\n",
            "}\r\n",
        ),
    },
    Pin {
        fixture: "claustrum with a key over a CRLF file",
        bytes: concat!(
            "{\r\n",
            "  // a CRLF file\r\n",
            "  \"version\": 1,\r\n",
            "  \"modules\": {\r\n",
            "    \"aft\": { \"program\": \"/user/aft\" }, // aft\r\n",
            "    \"claustrum\": {\r\n",
            "      \"env\": {\r\n",
            "        \"CK_MASTER_KEY_PATH\": \"/opt/ck/keys/master.key\"\r\n",
            "      },\r\n",
            "      \"program\": @ck-claustrum@,\r\n",
            "      // without it any local process completing the handshake can claim the vault's module id and be handed bearer capability handles\r\n",
            "      \"reserved\": true\r\n",
            "    }\r\n",
            "  }\r\n",
            "}\r\n",
        ),
    },
    Pin {
        fixture: "fresh core",
        bytes: concat!(
            "{\n",
            "  \"storage\": {\n",
            "    \"backend\": \"sqlite\"\n",
            "  },\n",
            "  \"version\": 1\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "fresh aft",
        bytes: concat!(
            "{\n",
            "  \"modules\": {\n",
            "    \"aft\": {\n",
            "      \"program\": @ck-aft@\n",
            "    }\n",
            "  }\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "fresh mc",
        bytes: concat!(
            "{\n",
            "  \"modules\": {\n",
            "    \"magic-context\": {\n",
            "      \"program\": @ck-mc@\n",
            "    }\n",
            "  }\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "fresh insula",
        bytes: concat!(
            "{\n",
            "  \"modules\": {\n",
            "    \"insula\": {\n",
            "      \"program\": @ck-insula@\n",
            "    }\n",
            "  }\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "fresh claustrum",
        bytes: concat!(
            "{\n",
            "  \"modules\": {\n",
            "    \"claustrum\": {\n",
            "      \"program\": @ck-claustrum@,\n",
            "      // without it any local process completing the handshake can claim the vault's module id and be handed bearer capability handles\n",
            "      \"reserved\": true\n",
            "    }\n",
            "  }\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "fresh claustrum with a key",
        bytes: concat!(
            "{\n",
            "  \"modules\": {\n",
            "    \"claustrum\": {\n",
            "      \"env\": {\n",
            "        \"CK_MASTER_KEY_PATH\": \"/opt/ck/keys/master.key\"\n",
            "      },\n",
            "      \"program\": @ck-claustrum@,\n",
            "      // without it any local process completing the handshake can claim the vault's module id and be handed bearer capability handles\n",
            "      \"reserved\": true\n",
            "    }\n",
            "  }\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "fresh synapse",
        bytes: concat!(
            "{\n",
            "  \"modules\": {\n",
            "    \"synapse\": {\n",
            "      \"program\": @ck-synapse@\n",
            "    }\n",
            "  }\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "claustrum over a written aft file",
        bytes: concat!(
            "{\n",
            "  \"modules\": {\n",
            "    \"aft\": {\n",
            "      \"program\": @ck-aft@\n",
            "    },\n",
            "    \"claustrum\": {\n",
            "      \"program\": @ck-claustrum@,\n",
            "      // without it any local process completing the handshake can claim the vault's module id and be handed bearer capability handles\n",
            "      \"reserved\": true\n",
            "    }\n",
            "  }\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "claustrum with a key over a written aft file",
        bytes: concat!(
            "{\n",
            "  \"modules\": {\n",
            "    \"aft\": {\n",
            "      \"program\": @ck-aft@\n",
            "    },\n",
            "    \"claustrum\": {\n",
            "      \"env\": {\n",
            "        \"CK_MASTER_KEY_PATH\": \"/opt/ck/keys/master.key\"\n",
            "      },\n",
            "      \"program\": @ck-claustrum@,\n",
            "      // without it any local process completing the handshake can claim the vault's module id and be handed bearer capability handles\n",
            "      \"reserved\": true\n",
            "    }\n",
            "  }\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "bus health check over a hand-placed bus",
        bytes: concat!(
            "{\n",
            "  // the operator's comment survives\n",
            "  \"version\": 1,\n",
            "  \"modules\": {\n",
            "    \"ckbus\": { \"program\": \"/opt/bus/ck-bus\" },\n",
            "    \"nats-server\": {\n",
            "      \"program\": \"/opt/homebrew/bin/nats-server\",\n",
            "      \"args\": [\"-c\", \"/opt/bus/nats/server.conf\"],\n",
            "      \"protocol\": \"none\",\n",
            "      \"health\": {\n",
            "        \"cadence_ms\": 30000,\n",
            "        \"deadline_ms\": 5000,\n",
            "        \"http\": \"http://127.0.0.1:18222/healthz\"\n",
            "      }\n",
            "    }\n",
            "  }\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "rollback after an edited comment",
        bytes: concat!(
            "{\n",
            "  // edited by operator\n",
            "  \"version\": 1,\n",
            "  \"modules\": {}\n",
            "}\n",
        ),
    },
    Pin {
        fixture: "rollback beside an operator's sibling",
        bytes: concat!(
            "{\n",
            "  \"version\": 1,\n",
            "  \"storage\": {\n",
            "     \"data_home\": \"/custom\" // operator setting\n",
            "  }\n",
            "}\n",
        ),
    },
];
