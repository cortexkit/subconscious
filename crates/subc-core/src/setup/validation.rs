use std::path::Path;

use super::model::Component;

pub trait Validator {
    fn run(&mut self, label: &str, args: &[String]) -> Result<bool, String>;
}

/// Existing ck interfaces are the post-setup evidence. Runtime registration and
/// current liveness are intentionally validated separately by the runtime layer.
pub fn validate_selected<V: Validator>(
    validator: &mut V,
    selected: &[Component],
    config_path: &Path,
) -> Result<(), String> {
    require(
        validator,
        "ck",
        &["daemon".to_string(), "triage".to_string()],
    )?;
    for component in selected {
        let args = match component {
            Component::Core => vec!["health".to_string()],
            Component::Aft => vec!["health".to_string(), "aft".to_string()],
            Component::Mc => vec!["health".to_string(), "mc".to_string()],
        };
        require(validator, "ck", &args)?;
    }
    require(
        validator,
        "ck",
        &[
            "fleet".to_string(),
            "lint".to_string(),
            config_path.to_string_lossy().into_owned(),
        ],
    )
}

pub const MCP_HARNESS_SNIPPET: &str = "MCP harness snippet:\n  ck-subc-mcp --harness ck";

fn require<V: Validator>(validator: &mut V, label: &str, args: &[String]) -> Result<(), String> {
    if validator.run(label, args)? {
        Ok(())
    } else {
        Err(format!("validation failed: {label} {}", args.join(" ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingValidator {
        calls: Vec<Vec<String>>,
    }
    impl Validator for RecordingValidator {
        fn run(&mut self, _label: &str, args: &[String]) -> Result<bool, String> {
            self.calls.push(args.to_vec());
            Ok(true)
        }
    }

    #[test]
    fn validation_uses_triage_health_and_fleet_lint_interfaces() {
        let mut validator = RecordingValidator::default();
        validate_selected(
            &mut validator,
            &[Component::Core, Component::Aft],
            Path::new("/config/subc.jsonc"),
        )
        .expect("healthy fixture");
        assert_eq!(validator.calls[0], ["daemon", "triage"]);
        assert!(validator
            .calls
            .contains(&vec!["health".to_string(), "aft".to_string()]));
        assert!(validator
            .calls
            .last()
            .expect("fleet lint")
            .starts_with(&["fleet".to_string(), "lint".to_string()]));
    }
}
