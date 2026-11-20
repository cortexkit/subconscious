use super::SupervisedChild;
use std::io;

pub(super) async fn stop_unregistered_child(child: &mut SupervisedChild) -> io::Result<()> {
    child.retire_registration();
    if let Some(container) = child.child.container() {
        container.stop_members().await;
        child.kill_leader()?;
        child.wait().await?;
        return Ok(());
    }
    #[cfg(unix)]
    if let Some(group) = &child.child.plain_group {
        group.settle().await;
        child.wait().await?;
        return Ok(());
    }
    #[cfg(unix)]
    {
        use rustix::process::{kill_process_group, Signal};
        // Preserve the registration-failure grace for placed Linux modules
        // while keeping the spawn's own leader unreaped through both signals.
        let group = unreaped_registration_group(child)?;
        let _ = kill_process_group(group, Signal::TERM);
        tokio::time::sleep(crate::child_roster::TERM_TO_KILL).await;
        let group = unreaped_registration_group(child)?;
        let _ = kill_process_group(group, Signal::KILL);
    }
    // Windows terminates the job; placed Linux modules kill the cgroup.
    child.start_kill()?;
    child.wait().await?;
    Ok(())
}

#[cfg(unix)]
fn unreaped_registration_group(child: &SupervisedChild) -> io::Result<rustix::process::Pid> {
    let group = i32::try_from(child.pid)
        .ok()
        .filter(|pid| *pid > 1)
        .and_then(rustix::process::Pid::from_raw)
        .filter(|_| child.child.id() == Some(child.pid))
        .ok_or_else(|| io::Error::other("refusing signal without the unreaped spawn leader"))?;
    subc_os::child_exit::observe(child.pid)?;
    Ok(group)
}
