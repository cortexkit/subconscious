use ck_ui::model::{ProgramUsage, ResourceRow, ResourceRowKind, ResourceUsage};
use ck_ui::Snapshot;
use subc_control::CpuLastHour;

pub fn hour(cores: f64) -> CpuLastHour {
    hour_window((cores * 3_600_000.0).round() as u64, 3_600_000, false)
}

pub fn hour_window(cpu_ms: u64, covered_ms: u64, lower: bool) -> CpuLastHour {
    serde_json::from_value(serde_json::json!({
        "cpu_ms": cpu_ms,
        "covered_ms": covered_ms,
        "bound": lower.then_some("lower"),
    }))
    .unwrap()
}

pub fn program(name: &str, count: u64, cpu: f64, mem: f64) -> ProgramUsage {
    ProgramUsage {
        name: name.into(),
        count,
        cpu_percent: Some(cpu),
        memory_mb: mem,
        lower: false,
    }
}

/// Resource values assigned to the existing Modules fixture roster.
pub fn snapshot() -> Snapshot {
    let mut snap = Snapshot::fixture();
    let cores = [
        0.21, 0.01, 0.02, 0.21, 0.0, 0.01, 0.0, 0.0, 0.0, 0.0, 0.04, 0.01, 0.0, 0.0, 0.03, 0.02,
        0.0, 0.01, 0.12, 0.01, 0.0, 0.07, 0.01, 0.05, 0.0,
    ];
    for (m, cores) in snap.modules.data.iter_mut().zip(cores) {
        m.resources = Some(ResourceUsage {
            cpu_percent: m.cpu_percent,
            memory_mb: m.memory_mb,
            last_hour: Some(hour(cores)),
            process_count: Some(1),
            own: Some(program(
                "own process",
                1,
                m.cpu_percent.unwrap(),
                m.memory_mb.unwrap(),
            )),
            cpu_history: m.cpu_history.clone(),
            memory_history: m.memory_history.clone(),
            ..ResourceUsage::default()
        });
    }
    let first = snap.modules.data[0].resources.as_mut().unwrap();
    first.cpu_percent = Some(15.2);
    first.memory_mb = Some(4400.0);
    first.own = Some(program("own process", 1, 3.4, 2970.0));
    first.groups = vec![program("rust-analyzer", 1, 11.8, 1430.0)];
    first.process_count = Some(2);
    let worker = snap.modules.data[21].resources.as_mut().unwrap();
    worker.memory_mb = Some(895.0);
    worker.groups = vec![program("worker", 1, 0.0, 65.0)];
    worker.process_count = Some(2);
    let harness = snap.modules.data[23].resources.as_mut().unwrap();
    harness.cpu_percent = Some(1.4);
    harness.memory_mb = Some(651.0);
    harness.groups = vec![program("opencode", 2, 1.3, 610.0)];
    harness.process_count = Some(3);
    for m in &mut snap.modules.data {
        let usage = m.resources.as_mut().unwrap();
        let cpu = usage.cpu_percent.unwrap();
        let mem = usage.memory_mb.unwrap();
        usage.cpu_history = series(m.pid.unwrap() % 97, cpu, (cpu * 0.5).max(0.3));
        usage.memory_history = series(m.pid.unwrap() % 89, mem, mem * 0.04);
    }
    snap.resource_rows = vec![
        ResourceRow {
            kind: ResourceRowKind::Agents {
                via: snap.modules.data[0].id.clone(),
            },
            usage: ResourceUsage {
                cpu_percent: Some(181.4),
                memory_mb: Some(2230.0),
                last_hour: Some(hour(1.41)),
                process_count: Some(4),
                groups: vec![
                    program("compiler", 3, 180.0, 2200.0),
                    program("sh", 1, 1.4, 30.0),
                ],
                ..ResourceUsage::default()
            },
            started_at: None,
            build: String::new(),
        },
        ResourceRow {
            kind: ResourceRowKind::Daemon,
            usage: ResourceUsage {
                cpu_percent: Some(1.9),
                memory_mb: Some(117.0),
                last_hour: Some(hour(0.02)),
                process_count: Some(1),
                own: Some(program("own process", 1, 1.9, 117.0)),
                ..ResourceUsage::default()
            },
            started_at: Some(Snapshot::fixture_now().add_secs(-9.2 * 3600.0)),
            build: "0.40.0".into(),
        },
    ];
    snap
}

fn series(seed: u32, base: f64, jitter: f64) -> Vec<f64> {
    let mut s = seed;
    (0..60)
        .map(|i| {
            s = (s * 9301 + 49297) % 233280;
            (base
                + (f64::from(s) / 233280.0 - 0.5) * 2.0 * jitter
                + (f64::from(i) / 3.0 + f64::from(seed)).sin() * jitter * 0.6)
                .max(0.0)
        })
        .collect()
}

pub const FRAMES: &[&str] = &[
    "modules-resources-120x36",
    "modules-resources-80x24",
    "modules-resources-160x48",
    "modules-lower-120x36",
    "modules-hour-lower-120x36",
    "modules-hour-warming-120x36",
    "modules-agents-120x36",
    "modules-daemon-120x36",
    "modules-not-measured-120x36",
    "modules-no-own-120x36",
];

pub fn frame(name: &str) -> (Snapshot, usize) {
    let mut snap = snapshot();
    let mut selected = 0;
    match name {
        "modules-lower-120x36" => {
            let usage = snap.modules.data[0].resources.as_mut().unwrap();
            usage.lower = true;
            usage.groups[0].lower = true;
            usage.own.as_mut().unwrap().lower = true;
        }
        "modules-hour-lower-120x36" => {
            let h = snap.modules.data[0]
                .resources
                .as_mut()
                .unwrap()
                .last_hour
                .as_mut()
                .unwrap();
            h.cpu_ms = 0;
            h.bound = Some(subc_control::ResourceBound::Lower);
        }
        "modules-hour-warming-120x36" => {
            let h = snap.modules.data[0]
                .resources
                .as_mut()
                .unwrap()
                .last_hour
                .as_mut()
                .unwrap();
            h.cpu_ms /= 2;
            h.covered_ms /= 2;
        }
        "modules-agents-120x36" => selected = snap.modules.data.len(),
        "modules-daemon-120x36" => selected = snap.modules.data.len() + 1,
        "modules-not-measured-120x36" => {
            snap.modules.data[0].resources = Some(ResourceUsage {
                unavailable: Some("not_yet_measured".into()),
                ..ResourceUsage::default()
            })
        }
        "modules-no-own-120x36" => snap.modules.data[0].resources.as_mut().unwrap().own = None,
        "modules-resources-120x36" | "modules-resources-80x24" | "modules-resources-160x48" => {}
        _ => panic!("unknown resource frame {name}"),
    }
    (snap, selected)
}
