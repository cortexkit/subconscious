use super::jittered_health_delay;
use std::{collections::HashSet, time::Duration};

/// Module ids drawn from a real fleet, so the dispersal claim is about names
/// that actually occur rather than invented ones.
///
/// This is a SAMPLE, not a registry: the property under test is that distinct
/// ids disperse, which holds for any set of distinct strings. Several entries
/// are already historical (modules get renamed), and that costs nothing here --
/// but it means a reader must not mistake this for the live module set, and a
/// rename sweep will match it without there being anything to change.
const FLEET: [&str; 14] = [
    "aft",
    "alfonso-core",
    "magic-context",
    "broca",
    "thalamus",
    "quota",
    "engram",
    "plexus",
    "cerebellum",
    "astrocyte",
    "synapse",
    "subc-mcp",
    "cortexkit-credentials",
    "subc-federation",
];

/// Probes must not converge after a fleet-wide restart.
///
/// This is the property the jitter exists for: every module reconnects at
/// once, and without dispersal all fourteen would then probe on the same
/// tick forever. Nothing failed visibly when this went untested -- a
/// convergent fleet still probes correctly, just in a burst, so the symptom
/// is a periodic load spike that looks like whatever else is running.
#[test]
fn probe_delays_disperse_across_the_fleet() {
    let cadence = Duration::from_secs(30);
    let delays: HashSet<Duration> = FLEET
        .iter()
        .map(|id| jittered_health_delay(id, 0, cadence))
        .collect();
    assert_eq!(
        delays.len(),
        FLEET.len(),
        "every supervised module must land on its own probe offset"
    );
}

/// The offset may only ever DELAY a probe, never bring it forward.
///
/// A delay below the cadence would probe a module more often than
/// configured, which is the opposite of what an operator asked for and
/// would tighten the failure budget without anyone changing it.
#[test]
fn jitter_only_delays_and_stays_within_one_tenth_of_cadence() {
    let cadence = Duration::from_secs(30);
    let span = cadence / 10;
    for id in FLEET {
        for probe_index in 0..8 {
            let delay = jittered_health_delay(id, probe_index, cadence);
            assert!(
                delay >= cadence,
                "{id}#{probe_index}: jitter must not shorten the cadence"
            );
            assert!(
                delay < cadence + span,
                "{id}#{probe_index}: jitter must stay inside one tenth of the cadence"
            );
        }
    }
}

/// A module keeps its offset across daemon restarts.
///
/// The delay is derived rather than randomised precisely so a restart does
/// not re-roll every module into a fresh chance of collision. A random
/// source would satisfy the dispersal test above and quietly lose this.
#[test]
fn a_module_offset_is_stable_across_restarts() {
    let cadence = Duration::from_secs(30);
    for id in FLEET {
        assert_eq!(
            jittered_health_delay(id, 0, cadence),
            jittered_health_delay(id, 0, cadence),
            "{id}: the same module and probe index must produce the same offset"
        );
    }
}

/// A zero cadence disables probing rather than producing a busy loop.
#[test]
fn zero_cadence_yields_zero_delay() {
    assert_eq!(
        jittered_health_delay("aft", 0, Duration::ZERO),
        Duration::ZERO
    );
}
