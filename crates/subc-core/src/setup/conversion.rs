use std::collections::BTreeSet;

use super::model::{Component, SetupRequest};

/// Selects components through the ordinary component-add path. A confirmed
/// explicit conversion contributes its component to this same set, so it cannot
/// acquire a separate installation or configuration path.
pub fn selected_components(request: &SetupRequest) -> BTreeSet<Component> {
    let mut selected = BTreeSet::from([Component::Core]);
    selected.extend(request.optional_components.iter().copied());
    if let Some(component) = request.convert {
        selected.insert(component);
    }
    selected
}

/// An explicit conversion may be planned for review, but authorization remains
/// absent until the caller has supplied the confirmation flag.
pub const fn explicit_conversion_requires_confirmation(request: &SetupRequest) -> bool {
    request.convert.is_some() && !request.conversion_confirmed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_conversion_selects_the_same_component_as_component_addition() {
        let add = SetupRequest::install(vec![Component::Mc]);
        let mut convert = SetupRequest::install(Vec::new());
        convert.convert = Some(Component::Mc);
        convert.conversion_confirmed = true;

        assert_eq!(selected_components(&add), selected_components(&convert));
    }

    #[test]
    fn an_unconfirmed_explicit_conversion_is_not_authorized() {
        let mut request = SetupRequest::install(Vec::new());
        request.convert = Some(Component::Mc);
        assert!(explicit_conversion_requires_confirmation(&request));
        request.conversion_confirmed = true;
        assert!(!explicit_conversion_requires_confirmation(&request));
    }
}
