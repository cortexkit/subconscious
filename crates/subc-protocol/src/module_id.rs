//! Shared module-id grammar for identity and declaration keys.

/// Why `module_id` cannot serve as one literal store-path component.
///
/// This refuses path hazards rather than sanitizing identities. Colons are
/// allowed for reserved-namespace children such as `mcp:worker`; platform-specific
/// filename legality belongs to the store, not to the identity grammar.
/// The daemon's copy in `registry.rs` is to be replaced by a call to this helper.
pub fn module_id_path_hazard(module_id: &str) -> Result<(), String> {
    if module_id.is_empty() {
        return Err("empty".to_string());
    }
    if module_id.contains('/') || module_id.contains('\\') {
        return Err("contains a path separator".to_string());
    }
    if module_id == "." || module_id == ".." {
        return Err("is a dot path component".to_string());
    }
    if module_id.chars().any(|c| c.is_control()) {
        return Err("contains a control character".to_string());
    }
    // The 255-byte store-path component limit counts UTF-8 bytes, not characters.
    if module_id.len() > 255 {
        return Err("is longer than 255 bytes".to_string());
    }
    Ok(())
}
