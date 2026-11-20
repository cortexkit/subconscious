use super::swap::cgroup_name;

pub(super) fn generation_label(module_id: &str, alternate_slot: bool, suffix: u128) -> String {
    const NAME_MAX: usize = 255;
    let suffix = suffix.to_string();
    // Placement adds `m-` and escapes each underscore as `_5f`, including
    // the one before the nonce and the one in the optional `_swap` suffix.
    let mut remaining = NAME_MAX - 2 - 3 - suffix.len() - if alternate_slot { 7 } else { 0 };
    let mut end = 0;
    for ch in module_id.chars() {
        let width = ch.len_utf8();
        let encoded = if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.') {
            1
        } else {
            // The first encoder emits `_xx` per byte, the second `_5fxx`.
            5 * width
        };
        if end + width > 64 || encoded > remaining {
            break;
        }
        end += width;
        remaining -= encoded;
    }
    format!(
        "{}_{suffix}",
        cgroup_name(&module_id[..end], alternate_slot)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_label_fits_after_both_encodings() {
        let root = cortexkit_test_support::ScratchDir::new("cgroup-label");
        std::fs::write(root.join("cgroup.procs"), []).unwrap();
        let placement = subc_cgroup::prepare_at(&root).unwrap().unwrap();
        for id in ["é".repeat(32), "_".repeat(64)] {
            assert_eq!(id.len(), 64);
            subc_protocol::module_id::module_id_path_hazard(&id).unwrap();
            for alternate in [false, true] {
                let label = generation_label(&id, alternate, u128::MAX);
                let path = placement.module_path(&label).unwrap_or_else(|err| {
                    panic!("{id:?} (alternate: {alternate}) did not fit: {err}")
                });
                let component = path.file_name().unwrap().to_str().unwrap();
                let expected_end = if alternate { "_5fswap_5f" } else { "_5f" };
                assert!(component.ends_with(&format!("{expected_end}{}", u128::MAX)));
                assert!(component.len() <= 255);
            }
        }
    }
}
