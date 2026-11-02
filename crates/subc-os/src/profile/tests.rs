use super::*;

#[test]
fn profile_dir_ignores_hostile_environment() {
    const PROBE: &str = "SUBC_OS_PROFILE_DIR_PROBE";
    const MARKER: &str = "account-profile=";
    if std::env::var_os(PROBE).is_some() {
        let directory = profile_dir().unwrap();
        assert!(directory.is_absolute(), "{directory:?}");
        assert!(directory.is_dir(), "{directory:?}");
        println!("\n{MARKER}{directory:?}");
        return;
    }
    let run = |hostile: bool| {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "profile::tests::profile_dir_ignores_hostile_environment",
                "--nocapture",
            ])
            .env(PROBE, "1");
        if hostile {
            for (name, value) in [
                ("USERPROFILE", r"C:\untrusted\prof"),
                ("LOCALAPPDATA", r"C:\untrusted\local"),
                ("HOMEDRIVE", "Z:"),
                ("HOMEPATH", r"\untrusted\home"),
                ("SystemDrive", "Z:"),
                ("SystemRoot", r"Z:\untrusted\windows"),
            ] {
                child.env(name, value);
            }
        }
        let output = child.output().unwrap();
        assert!(output.status.success(), "child failed: {output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.contains("1 passed"),
            "profile probe must run: {stdout}"
        );
        stdout
            .lines()
            .find_map(|line| line.strip_prefix(MARKER))
            .expect("child must print its account profile")
            .to_owned()
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn profile_value_requires_a_literal_absolute_path() {
    let value = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    assert_eq!(
        profile_from_value(&value(r"C:\Users\account")).unwrap(),
        PathBuf::from(r"C:\Users\account")
    );
    for (text, reason) in [
        ("", "empty"),
        (r"Users\account", "not absolute"),
        (r"C:Users\account", "not absolute"),
        (r"%SystemDrive%\Users\account", "contains '%'"),
        (r"C:\Users\%USERNAME%", "contains '%'"),
        ("C:\\Users\\account\0suffix", "embedded null"),
    ] {
        let error = profile_from_value(&value(text)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains(reason), "{text:?}: {error}");
    }
    assert!(profile_from_value(&[]).is_err());
    assert!(profile_from_value(&[u16::from(b'C')]).is_err());
}
