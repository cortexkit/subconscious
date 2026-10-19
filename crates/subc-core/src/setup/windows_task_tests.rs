//! Native Task Scheduler checks use unique temporary task names, never the daemon task.
use super::*;
use cortexkit_test_support::ScratchDir;

fn powershell(script: String) -> CommandResult {
    SystemCommandRunner
        .run(
            "powershell.exe",
            &[
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script,
            ],
        )
        .expect("run Windows PowerShell")
}

struct TemporaryTask(String);
impl TemporaryTask {
    fn cleanup(&self) -> CommandResult {
        powershell(format!("$ErrorActionPreference='Stop'; Stop-ScheduledTask -TaskPath '\\' -TaskName '{}' -ErrorAction SilentlyContinue; Unregister-ScheduledTask -TaskPath '\\' -TaskName '{}' -Confirm:$false -ErrorAction SilentlyContinue; if(Get-ScheduledTask -TaskPath '\\' -TaskName '{}' -ErrorAction SilentlyContinue) {{ throw 'temporary task remains registered' }}", self.0, self.0, self.0))
    }
}
impl Drop for TemporaryTask {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[test]
fn native_windows_task_export_update_and_ci_live_instance() {
    let root = ScratchDir::new("native-task-definition");
    let paths = RuntimePaths {
        definition: root.join("définition.xml"),
        daemon: PathBuf::from(std::env::var_os("SystemRoot").expect("Windows system root"))
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe"),
    };
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let task = TemporaryTask(format!(
        "cortexkit-definition-test-{}-{unique}",
        std::process::id()
    ));
    let platform = RuntimePlatform::Windows;
    let desired = desired_definition_checked(platform, &paths)
        .unwrap()
        .replace(platform.identifier(), &format!("\\{}", task.0));
    let old = legacy_definitions(platform, &paths, &desired)[1].clone();
    let register = || {
        let result = SystemCommandRunner
            .run(
                "schtasks.exe",
                &windows_create_arguments(&paths, &format!("\\{}", task.0)),
            )
            .unwrap();
        assert!(
            result.success,
            "register temporary task: {}",
            result.explanation()
        );
    };
    let assert_export = |expected: &str| {
        let exported = export_windows_task("\\", &task.0, &mut SystemCommandRunner).unwrap();
        assert!(
            exported.success,
            "export temporary task: {}",
            exported.explanation()
        );
        assert_eq!(
            canonical_registered_windows(&exported.stdout).unwrap(),
            canonical_registered_windows(expected).unwrap(),
            "native registered values must match the submitted definition"
        );
    };
    fs::write(&paths.definition, &old).unwrap();
    register();
    assert_export(&old);
    fs::write(&paths.definition, &desired).unwrap();
    update_windows_task_at(&paths, "\\", &task.0, &mut SystemCommandRunner).unwrap();
    assert_export(&desired);

    // Require the live-process proof on GitHub's Windows runners, including the
    // interactive logon that the daemon task requires. Other headless build
    // servers can check registration/export/update without a desktop token.
    if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
        let marker = root
            .join("running-pid")
            .to_string_lossy()
            .replace('\'', "''");
        let arguments = format!(
            r#"-NoProfile -NonInteractive -Command "[IO.File]::WriteAllText('{marker}', [string]$PID); Start-Sleep -Seconds 180""#
        );
        let running = old.replace(
            "</Command>",
            &format!("</Command><Arguments>{}</Arguments>", xml_text(&arguments)),
        );
        fs::write(&paths.definition, &running).unwrap();
        register();
        let started = powershell(format!("$ErrorActionPreference='Stop'; Start-ScheduledTask -TaskPath '\\' -TaskName '{}'; $watch=[Diagnostics.Stopwatch]::StartNew(); while(-not(Test-Path -LiteralPath '{marker}')) {{ if($watch.Elapsed.TotalSeconds -gt 20) {{ throw ('temporary task did not start: ' + ((Get-ScheduledTaskInfo -TaskPath '\\' -TaskName '{}') | ConvertTo-Json -Compress)) }}; Start-Sleep -Milliseconds 50 }}; $child=Get-Process -Id ([int](Get-Content -LiteralPath '{marker}')); @{{pid=$child.Id; start=$child.StartTime.ToFileTimeUtc()}} | ConvertTo-Json -Compress", task.0, task.0));
        assert!(
            started.success,
            "start temporary task: {}",
            started.explanation()
        );
        let before: Value = serde_json::from_str(started.stdout.trim()).unwrap();
        fs::write(&paths.definition, &desired).unwrap();
        update_windows_task_at(&paths, "\\", &task.0, &mut SystemCommandRunner).unwrap();
        assert_export(&desired);
        let observed = powershell(format!("$ErrorActionPreference='Stop'; $child=Get-Process -Id {}; @{{pid=$child.Id; start=$child.StartTime.ToFileTimeUtc(); state=[string](Get-ScheduledTask -TaskPath '\\' -TaskName '{}').State}} | ConvertTo-Json -Compress", before["pid"], task.0));
        assert!(
            observed.success,
            "original task process must survive: {}",
            observed.explanation()
        );
        let after: Value = serde_json::from_str(observed.stdout.trim()).unwrap();
        assert_eq!(after["pid"], before["pid"]);
        assert_eq!(after["start"], before["start"]);
        assert_eq!(after["state"], "Running");
    } else {
        eprintln!("live-instance proof requires GITHUB_ACTIONS=true and an interactive Windows logon; registration/export/update were checked");
    }
    let changed = powershell(format!("$ErrorActionPreference='Stop'; $t=Get-ScheduledTask -TaskPath '\\' -TaskName '{}'; $t.Settings.RestartCount=1; Set-ScheduledTask -InputObject $t | Out-Null", task.0));
    assert!(
        changed.success,
        "change only the registered retry count: {}",
        changed.explanation()
    );
    let edited = export_windows_task("\\", &task.0, &mut SystemCommandRunner).unwrap();
    assert!(
        edited.success,
        "export edited registration: {}",
        edited.explanation()
    );
    assert_ne!(
        canonical_registered_windows(&edited.stdout).unwrap(),
        canonical_registered_windows(&desired).unwrap(),
        "an actual task edit must differ even though the submitted XML file is unchanged"
    );
    let cleanup = task.cleanup();
    assert!(
        cleanup.success,
        "remove temporary task: {}",
        cleanup.explanation()
    );
}
