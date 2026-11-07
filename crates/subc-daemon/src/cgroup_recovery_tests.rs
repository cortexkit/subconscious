use super::*;
use crate::{
    child_roster::ChildRoster,
    live_children::{self, AdoptedPids, ContainerIdentity, LiveChild},
    run_dir_lock::RunDirLock,
};
use cortexkit_test_support::ScratchDir;
use subc_control::ModuleProtocol;

struct Fixture {
    root: ScratchDir,
    placement: Placement,
    module: PathBuf,
}
impl Fixture {
    fn new(label: &str) -> Self {
        let root = ScratchDir::new(label);
        fs::write(root.join("cgroup.procs"), "").unwrap();
        let placement = subc_cgroup::prepare_at(&root).unwrap().unwrap();
        let module = placement.module_path("owned").unwrap();
        fs::write(module.join("cgroup.kill"), "0").unwrap();
        fs::write(module.join("cgroup.events"), "populated 1\n").unwrap();
        // These are ordinary owned scratch files, not kernel interfaces. An
        // atomic-kill marker is present and no freezer is present, so policy
        // checks can never signal these illustrative PID values.
        fs::write(module.join("cgroup.procs"), "42\n").unwrap();
        Self {
            root,
            placement,
            module,
        }
    }
    fn entry(&self, path: &Path) -> LiveChild {
        LiveChild {
            module_id: "recorded-cgroup".into(),
            pid: 0,
            protocol: ModuleProtocol::None,
            start_time: None,
            executable: None,
            cgroup_name: Some("owned".into()),
            container: Some(ContainerIdentity {
                pgid: None,
                created_start_time: None,
                cgroup_path: Some(path.to_owned()),
                ..ContainerIdentity::test_metadata()
            }),
        }
    }
    fn roster(&self, path: &Path, placement: Option<Placement>, label: &str) -> ChildRoster {
        let record = self.root.join(label);
        live_children::write_record(&record, &[self.entry(path)]).unwrap();
        let roster = ChildRoster::default();
        roster.set_cgroup_placement(placement);
        roster.record_to(record);
        roster
    }
    fn empty(&self) {
        fs::write(self.module.join("cgroup.procs"), "").unwrap();
        fs::write(self.module.join("cgroup.events"), "populated 0\n").unwrap();
    }
}

#[test]
fn populated_recorded_cgroup_blocks_until_kernel_reports_empty() {
    let fixture = Fixture::new("populated-carried-cgroup-record");
    let roster = fixture.roster(
        &fixture.module,
        Some(fixture.placement.clone()),
        "record.json",
    );
    assert_eq!(
        roster.previous_instance("recorded-cgroup"),
        Some((0, RetainedReason::ContainerSurvivors(vec![42])))
    );
    assert_eq!(fs::read(fixture.module.join("cgroup.kill")).unwrap(), b"1");
    assert_eq!(
        roster
            .retained_view()
            .waiting_detail("recorded-cgroup")
            .unwrap()
            .1,
        "waiting: container survivors (pids [42])"
    );
    fs::write(fixture.module.join("cgroup.procs"), "").unwrap();
    assert!(
        roster.previous_instance("recorded-cgroup").is_some(),
        "a non-frozen empty process list must not overrule populated 1"
    );
    fixture.empty();
    assert_eq!(roster.previous_instance("recorded-cgroup"), None);
    assert_eq!(
        roster.retained_view().waiting_detail("recorded-cgroup"),
        None
    );
    // Ordinary interface stand-ins prevent rmdir. That is a persisted cleanup
    // retry, not a live-holder block; a kernel cgroup has no regular files.
    let (_, reasons) =
        live_children::read_record_with_reasons(&fixture.root.join("record.json")).unwrap();
    assert_eq!(
        reasons.get(&0),
        Some(&RetainedReason::EmptyContainerDirectory)
    );
    assert!(fixture.module.is_dir());
}

#[test]
fn untrusted_recorded_cgroup_blocks_without_touching_its_control_files() {
    let fixture = Fixture::new("untrusted-carried-cgroup-record");
    let outside = fixture.root.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("cgroup.kill"), "untouched").unwrap();
    fs::write(outside.join("cgroup.freeze"), "untouched").unwrap();
    fs::write(outside.join("cgroup.events"), "populated 1\n").unwrap();
    fs::write(outside.join("cgroup.procs"), "999\n").unwrap();
    for (index, placement) in [Some(fixture.placement.clone()), None]
        .into_iter()
        .enumerate()
    {
        let roster = fixture.roster(&outside, placement, &format!("record-{index}.json"));
        assert_eq!(
            roster.previous_instance("recorded-cgroup"),
            Some((
                0,
                RetainedReason::ContainerUnverifiable("container root unavailable".into())
            ))
        );
        assert!(roster
            .retained_view()
            .waiting_detail("recorded-cgroup")
            .unwrap()
            .1
            .contains("container root unavailable"));
        assert_eq!(fs::read(outside.join("cgroup.kill")).unwrap(), b"untouched");
        assert_eq!(
            fs::read(outside.join("cgroup.freeze")).unwrap(),
            b"untouched"
        );
    }
}

#[test]
fn nested_cgroup_member_keeps_the_recorded_container_blocked() {
    let fixture = Fixture::new("nested-carried-cgroup-record");
    fs::write(fixture.module.join("cgroup.procs"), "").unwrap();
    let nested = fixture.module.join("child");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("cgroup.procs"), "43\n").unwrap();
    let roster = fixture.roster(
        &fixture.module,
        Some(fixture.placement.clone()),
        "record.json",
    );
    assert_eq!(
        roster.previous_instance("recorded-cgroup"),
        Some((0, RetainedReason::ContainerSurvivors(vec![43])))
    );
    assert_eq!(
        roster
            .retained_view()
            .waiting_detail("recorded-cgroup")
            .unwrap()
            .1,
        "waiting: container survivors (pids [43])"
    );
    fs::write(nested.join("cgroup.procs"), "").unwrap();
    fixture.empty();
    assert_eq!(roster.previous_instance("recorded-cgroup"), None);
}

#[tokio::test]
async fn boot_sweep_uses_the_recorded_cgroup_even_without_a_live_leader() {
    let fixture = Fixture::new("boot-carried-cgroup-record");
    let entry = fixture.entry(&fixture.module);
    let record = fixture.root.join("record.json");
    live_children::write_record(&record, std::slice::from_ref(&entry)).unwrap();
    let owner = RunDirLock::acquire(&record).unwrap();
    let decisions = live_children::sweep_orphans_with_placement(
        &owner,
        &AdoptedPids::none(),
        SweepBounds {
            term_grace: Duration::ZERO,
            kill_bound: Duration::ZERO,
        },
        Some(&fixture.placement),
    )
    .await;
    assert_eq!(
        decisions,
        vec![(entry.clone(), SweepDecision::ContainerSurvivors(vec![42]))]
    );
    assert_eq!(fs::read(fixture.module.join("cgroup.kill")).unwrap(), b"1");
    assert_eq!(live_children::read_record(&record).unwrap(), vec![entry]);
}
