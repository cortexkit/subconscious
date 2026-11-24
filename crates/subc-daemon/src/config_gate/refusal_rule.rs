//! Plain-language wording for a settings value the admitted schema refuses.
//!
//! The desktop shows an edit refusal's message under the setting, so it has to
//! say which rule failed, such as "must be at most 90". Validation records the
//! rule on the refusal; this module only words it. The wording states what is
//! allowed and never repeats the submitted value.

use serde_json::Value;
use subc_config::{Bound, Refusal, RefusalCode, RefusalRule};

/// Kept when validation names no single rule.
pub(crate) const GENERIC: &str = "the edit is refused by the admitted schema";

/// Longest enum that is listed value by value; longer ones are counted.
const LISTED_VALUES: usize = 10;

/// One sentence naming the rule `refusal` failed. `layer` is the settings
/// layer's name as people see it, for example "user".
pub(crate) fn describe(refusal: &Refusal, layer: &str) -> String {
    match (refusal.code, refusal.rule.as_deref()) {
        (RefusalCode::LayerForbidden, _) => format!("cannot be set in {layer} settings"),
        (RefusalCode::CredentialShared, _) => {
            "cannot send the user's credential to a destination set here".into()
        }
        (RefusalCode::BoundViolated, Some(RefusalRule::Bound { bound })) => bound_rule(*bound),
        (RefusalCode::BoundViolated, _) => "may not move past the inherited value".into(),
        (_, Some(rule)) => wording(rule).unwrap_or_else(|| GENERIC.into()),
        (_, None) => GENERIC.into(),
    }
}

fn wording(rule: &RefusalRule) -> Option<String> {
    Some(match rule {
        RefusalRule::Type { expected } => type_rule(expected),
        RefusalRule::Const { value } => format!("must be exactly {value}"),
        RefusalRule::Enum { values } => enum_rule(values),
        RefusalRule::ClosedOption => "must be one of the options offered for this setting".into(),
        RefusalRule::Credential => "must be a vault reference".into(),
        RefusalRule::Variant => "must match one of the shapes the setting allows".into(),
        RefusalRule::Expansion => "must not contain {env:} or {file:} references".into(),
        RefusalRule::Reference => "must not be an env or file reference".into(),
        RefusalRule::MinLength { bound } => {
            format!("must be at least {bound} {}", plural(*bound, "character"))
        }
        RefusalRule::MaxLength { bound } => {
            format!("must be at most {bound} {}", plural(*bound, "character"))
        }
        RefusalRule::Pattern { pattern } => format!("must match the pattern {pattern}"),
        RefusalRule::Minimum { bound, exclusive } => {
            let wording = if *exclusive {
                "greater than"
            } else {
                "at least"
            };
            format!("must be {wording} {bound}")
        }
        RefusalRule::Maximum { bound, exclusive } => {
            let wording = if *exclusive { "less than" } else { "at most" };
            format!("must be {wording} {bound}")
        }
        RefusalRule::MinItems { bound } => {
            format!("must have at least {bound} {}", plural(*bound, "item"))
        }
        RefusalRule::MaxItems { bound } => {
            format!("must have at most {bound} {}", plural(*bound, "item"))
        }
        RefusalRule::UniqueItems => "must not list the same item twice".into(),
        RefusalRule::DuplicateId => "must not hold two items with the same id".into(),
        RefusalRule::Undeclared => "is not a setting this module declares".into(),
        RefusalRule::Required { .. } => "is required".into(),
        RefusalRule::Bound { bound } => bound_rule(*bound),
        _ => return None,
    })
}

fn bound_rule(bound: Bound) -> String {
    match bound {
        Bound::RaiseOnly => "may only raise the inherited value",
        Bound::LowerOnly => "may only lower the inherited value",
        Bound::SubsetOnly => "may only keep some of the inherited values",
        _ => "may not move past the inherited value",
    }
    .into()
}

fn type_rule(expected: &[String]) -> String {
    let name = |kind: &str| match kind {
        "object" => "an object",
        "array" => "a list",
        "string" => "text",
        "boolean" => "true or false",
        "number" => "a number",
        "integer" => "a whole number",
        "null" => "empty",
        _ => "a supported value",
    };
    let names: Vec<_> = expected.iter().map(|kind| name(kind)).collect();
    if names.is_empty() {
        return "must be a supported value".into();
    }
    format!("must be {}", names.join(" or "))
}

fn enum_rule(allowed: &[Value]) -> String {
    if allowed.len() > LISTED_VALUES {
        return format!("must be one of the {} allowed values", allowed.len());
    }
    let listed: Vec<_> = allowed.iter().map(Value::to_string).collect();
    match listed.as_slice() {
        [only] => format!("must be {only}"),
        _ => format!("must be one of {}", listed.join(", ")),
    }
}

fn plural(count: u64, noun: &str) -> String {
    if count == 1 {
        noun.into()
    } else {
        format!("{noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn says(code: RefusalCode, rule: Option<RefusalRule>) -> String {
        let refusal = Refusal::new(code, "/field", None);
        describe(
            &match rule {
                Some(rule) => refusal.with_rule(rule),
                None => refusal,
            },
            "user",
        )
    }

    fn value_rule(rule: RefusalRule) -> String {
        says(RefusalCode::ValueInvalid, Some(rule))
    }

    #[test]
    fn refusal_rule_words_numeric_bounds_and_types() {
        let maximum =
            |bound: serde_json::Number, exclusive| RefusalRule::Maximum { bound, exclusive };
        let minimum =
            |bound: serde_json::Number, exclusive| RefusalRule::Minimum { bound, exclusive };
        let half = serde_json::Number::from_f64(1.5).unwrap();
        assert_eq!(value_rule(maximum(90.into(), false)), "must be at most 90");
        assert_eq!(value_rule(minimum(20.into(), false)), "must be at least 20");
        assert_eq!(
            value_rule(minimum(0.into(), true)),
            "must be greater than 0"
        );
        assert_eq!(value_rule(maximum(half, true)), "must be less than 1.5");
        assert_eq!(
            value_rule(RefusalRule::Type {
                expected: vec!["integer".into()]
            }),
            "must be a whole number"
        );
        assert_eq!(
            value_rule(RefusalRule::Type {
                expected: vec!["string".into(), "integer".into()]
            }),
            "must be text or a whole number"
        );
    }

    #[test]
    fn refusal_rule_words_text_list_and_choice_rules() {
        assert_eq!(
            value_rule(RefusalRule::MinLength { bound: 2 }),
            "must be at least 2 characters"
        );
        assert_eq!(
            value_rule(RefusalRule::MaxLength { bound: 1 }),
            "must be at most 1 character"
        );
        assert_eq!(
            value_rule(RefusalRule::Pattern {
                pattern: "^[a-z]+$".into()
            }),
            "must match the pattern ^[a-z]+$"
        );
        assert_eq!(
            value_rule(RefusalRule::Enum {
                values: vec![json!("fast"), json!("slow")]
            }),
            r#"must be one of "fast", "slow""#
        );
        assert_eq!(
            value_rule(RefusalRule::Enum {
                values: (0..11).map(|n| json!(n)).collect()
            }),
            "must be one of the 11 allowed values"
        );
        assert_eq!(
            value_rule(RefusalRule::MaxItems { bound: 2 }),
            "must have at most 2 items"
        );
        assert_eq!(
            value_rule(RefusalRule::MinItems { bound: 1 }),
            "must have at least 1 item"
        );
        assert_eq!(
            value_rule(RefusalRule::Undeclared),
            "is not a setting this module declares"
        );
        assert_eq!(
            value_rule(RefusalRule::Required { key: "name".into() }),
            "is required"
        );
    }

    #[test]
    fn refusal_rule_words_codes_and_falls_back_without_a_rule() {
        assert_eq!(
            says(RefusalCode::LayerForbidden, None),
            "cannot be set in user settings"
        );
        assert_eq!(
            says(
                RefusalCode::BoundViolated,
                Some(RefusalRule::Bound {
                    bound: Bound::RaiseOnly
                })
            ),
            "may only raise the inherited value"
        );
        assert_eq!(says(RefusalCode::ValueInvalid, None), GENERIC);
    }
}
