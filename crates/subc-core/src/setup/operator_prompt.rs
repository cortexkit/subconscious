//! Installation of the one per-user Linux authentication policy.

use std::io;

#[cfg(target_os = "linux")]
fn administrator_script(uid: u32, uninstall: bool) -> String {
    let destination = subc_presence::polkit::action_path(uid);
    if uninstall {
        return format!("sudo rm -f -- '{destination}'\n");
    }
    format!(
        "policy=$(mktemp) || exit 1\ncat >\"$policy\" <<'CORTEXKIT_POLICY'\n{}CORTEXKIT_POLICY\nsudo install -m 0644 -o root -g root \"$policy\" '{destination}'\nstatus=$?\nrm -f -- \"$policy\"\n(exit \"$status\")\n",
        subc_presence::polkit::action_xml(uid)
    )
}

/// Interactive sudo inherits the terminal. Scripts get the complete policy and
/// command instead; no password prompt or privileged mutation happens unseen.
pub fn run(uninstall: bool, interactive: bool) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::io::Write;
        use std::process::Command;

        let uid = rustix::process::getuid().as_raw();
        if !interactive {
            println!("Run the following commands as the operator, with administrator approval:");
            print!("{}", administrator_script(uid, uninstall));
            return Ok(());
        }
        let destination = subc_presence::polkit::action_path(uid);
        let mut command = Command::new("sudo");
        // Keep the private temporary file until sudo has finished.
        let source = if uninstall {
            command.args(["rm", "-f", "--", &destination]);
            None
        } else {
            let mut source = tempfile::NamedTempFile::new()?;
            source.write_all(subc_presence::polkit::action_xml(uid).as_bytes())?;
            source.flush()?;
            command.args(["install", "-m", "0644", "-o", "root", "-g", "root"]);
            command.arg(source.path()).arg(&destination);
            Some(source)
        };
        let status = command.status()?;
        drop(source);
        if !status.success() {
            return Err(io::Error::other(format!(
                "operator prompt setup failed: {status}"
            )));
        }
        println!(
            "Operator prompt policy {}: {destination}",
            if uninstall { "removed" } else { "installed" }
        );
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (uninstall, interactive);
        println!("The operator prompt needs no setup on this platform.");
        Ok(())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn administrator_commands_touch_only_the_current_users_action() {
        let install = administrator_script(1234, false);
        assert!(install.contains("sudo install -m 0644 -o root -g root \"$policy\" '/usr/share/polkit-1/actions/io.cortexkit.subc.elevate.u1234.policy'"));
        assert!(install.contains(&subc_presence::polkit::action_xml(1234)));
        assert!(install.contains("policy=$(mktemp) || exit 1"));
        assert_eq!(
            administrator_script(1234, true),
            "sudo rm -f -- '/usr/share/polkit-1/actions/io.cortexkit.subc.elevate.u1234.policy'\n"
        );
    }
}
