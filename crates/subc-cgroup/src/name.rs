use std::fmt::Write as _;

pub(crate) fn module_directory_name(module_id: &str) -> String {
    // Reserve a namespace that cannot collide with cgroup interface filenames.
    let mut name = String::from("m-");
    for byte in module_id.bytes() {
        // Escape the escape marker too, so literal `_20` cannot alias a space.
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.') {
            name.push(char::from(byte));
        } else {
            let _ = write!(name, "_{byte:02x}");
        }
    }
    name
}
