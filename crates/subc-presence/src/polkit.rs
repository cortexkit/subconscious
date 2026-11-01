//! Per-user polkit policy and request text, also renderable on non-Linux hosts.

#[cfg(any(target_os = "linux", test))]
use std::collections::HashMap;

#[cfg(any(target_os = "linux", test))]
use crate::Outcome;

pub fn action_id(uid: u32) -> String {
    format!("io.cortexkit.subc.elevate.u{uid}")
}

pub fn action_path(uid: u32) -> String {
    format!("/usr/share/polkit-1/actions/{}.policy", action_id(uid))
}

/// Each operator owns only their action; authentication is never retained for
/// another request whose details might describe a different operation.
pub fn action_xml(uid: u32) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE policyconfig PUBLIC "-//freedesktop//DTD PolicyKit Policy Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/PolicyKit/1/policyconfig.dtd">
<policyconfig>
  <action id="{}">
    <description>Confirm a CortexKit operation</description>
    <message>Authenticate to confirm the requested CortexKit operation.</message>
    <defaults>
      <allow_any>no</allow_any>
      <allow_inactive>no</allow_inactive>
      <allow_active>auth_self</allow_active>
    </defaults>
    <annotate key="org.freedesktop.policykit.owner">unix-user:{uid}</annotate>
  </action>
</policyconfig>
"#,
        action_id(uid)
    )
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn details(text: &str) -> HashMap<&str, &str> {
    // polkit 126, polkitbackendinteractiveauthority.c:1990-2064 appends detail
    // values to a separate output buffer while scanning only the template.
    // Expansion is single-pass: $(...) inside the sentence stays literal.
    HashMap::from([
        ("polkit.message", "$(cortexkit.request)"),
        ("cortexkit.request", text),
    ])
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn preflight_outcome(authorized: bool, challenge: bool) -> Option<Outcome> {
    match (authorized, challenge) {
        // The generated policy's auth_self setting requires fresh authentication
        // as the subject's own user, with no retained grants. This probe did not
        // ask anyone, so an approval is an administrator's policy-rule grant.
        // Root's unconditional polkit grant is rejected before any bus call.
        (true, _) => Some(Outcome::Approved),
        (false, true) => None,
        (false, false) => Some(Outcome::NoPresence),
    }
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn outcome(authorized: bool, challenge: bool) -> Outcome {
    match (authorized, challenge) {
        (true, _) => Outcome::Approved,
        (false, true) => Outcome::NoPresence,
        (false, false) => Outcome::Declined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_user_policy_is_owned_and_never_retains_authorization() {
        let xml = action_xml(1234);
        assert!(xml.contains("id=\"io.cortexkit.subc.elevate.u1234\""));
        assert!(xml.contains("key=\"org.freedesktop.policykit.owner\">unix-user:1234<"));
        assert!(xml.contains("<allow_any>no</allow_any>"));
        assert!(xml.contains("<allow_inactive>no</allow_inactive>"));
        assert!(xml.contains("<allow_active>auth_self</allow_active>"));
        assert!(!xml.contains("_keep"));
        assert_ne!(action_path(1234), action_path(1235));
    }

    #[test]
    fn request_is_a_literal_detail_not_part_of_the_template() {
        let sentence = "Approve module summary $(x) & <literal> (requested by a local program)";
        let values = details(sentence);
        assert_eq!(values.len(), 2);
        assert_eq!(values["polkit.message"], "$(cortexkit.request)");
        assert_eq!(values["cortexkit.request"], sentence);
    }

    #[test]
    fn only_an_authorized_result_approves() {
        assert_eq!(outcome(true, false), Outcome::Approved);
        assert_eq!(outcome(true, true), Outcome::Approved);
        assert_eq!(outcome(false, true), Outcome::NoPresence);
        assert_eq!(outcome(false, false), Outcome::Declined);
    }

    #[test]
    fn policy_denial_without_a_challenge_is_not_a_persons_decline() {
        assert_eq!(preflight_outcome(false, false), Some(Outcome::NoPresence));
        assert_eq!(preflight_outcome(false, true), None);
        assert_eq!(preflight_outcome(true, false), Some(Outcome::Approved));
        assert_eq!(preflight_outcome(true, true), Some(Outcome::Approved));
    }
}
