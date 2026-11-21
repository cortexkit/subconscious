use super::*;

#[test]
fn a_private_scratch_path_value_round_trips_both_types_and_absence() {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = OsString::from(format!(
        "CortexKitJournalTest-{}-{suffix}",
        std::process::id()
    ));
    struct Remove(OsString);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = write_value(&self.0, None);
        }
    }
    let _remove = Remove(name.clone());
    assert_eq!(read_value(&name).unwrap(), None);
    for value in [
        UserPathValue::Literal("%USERPROFILE%;C:\\space folder".into()),
        UserPathValue::Expandable("%USERPROFILE%;C:\\space folder".into()),
    ] {
        write_value(&name, Some(&value)).unwrap();
        assert_eq!(read_value(&name).unwrap(), Some(value));
    }
    write_value(&name, None).unwrap();
    assert_eq!(read_value(&name).unwrap(), None);
}
