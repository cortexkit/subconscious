use super::ExitReport;

#[cfg(any(windows, test))]
pub(super) const WINDOWS_ADMISSION_KILL_EXIT_CODE: i32 = 0x434b_0001;

pub(super) fn is_admission_kill(kill_sent: bool, exit: &ExitReport) -> bool {
    #[cfg(unix)]
    return kill_sent && exit.signal == Some(9);
    #[cfg(windows)]
    return windows_admission_kill(kill_sent, exit);
    #[cfg(not(any(unix, windows)))]
    false
}

#[cfg(any(windows, test))]
fn windows_admission_kill(kill_sent: bool, exit: &ExitReport) -> bool {
    kill_sent && exit.code == Some(WINDOWS_ADMISSION_KILL_EXIT_CODE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervise::ExitKind;

    fn exit(code: Option<i32>, signal: Option<i32>) -> ExitReport {
        ExitReport {
            kind: ExitKind::Crash,
            code,
            signal,
            at_ms: 1,
        }
    }

    #[test]
    fn windows_admission_kill_does_not_match_ordinary_exit_one() {
        assert!(!windows_admission_kill(true, &exit(Some(1), None)));
    }

    #[test]
    fn windows_admission_kill_requires_sent_marker_and_reserved_status() {
        let killed = exit(Some(WINDOWS_ADMISSION_KILL_EXIT_CODE), None);
        assert!(windows_admission_kill(true, &killed));
        assert!(!windows_admission_kill(false, &killed));
        assert!(!windows_admission_kill(true, &exit(Some(23), None)));
    }

    #[cfg(unix)]
    #[test]
    fn unix_admission_kill_requires_sent_marker_and_sigkill() {
        let killed = exit(None, Some(9));
        assert!(is_admission_kill(true, &killed));
        assert!(!is_admission_kill(false, &killed));
        assert!(!is_admission_kill(true, &exit(Some(23), None)));
        assert!(!is_admission_kill(true, &exit(None, Some(15))));
    }
}
