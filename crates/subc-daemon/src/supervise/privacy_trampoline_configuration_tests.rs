#[tokio::test]
#[cfg_attr(not(target_os = "macos"), ignore = "requires macOS privacy trampoline")]
async fn macos_spawn_without_a_configured_trampoline_refuses_by_name() {
    #[cfg(target_os = "macos")]
    {
        let supervisor = super::Supervisor::new(
            std::sync::Arc::new(crate::Registry::default()),
            super::RestartPolicy::default(),
        );
        let error = supervisor.spawn(spec()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("no privacy trampoline configured"),
            "{error}"
        );
    }
}

#[tokio::test]
#[cfg_attr(not(target_os = "macos"), ignore = "requires macOS privacy trampoline")]
async fn macos_wrong_trampoline_is_rejected_by_the_startup_probe() {
    #[cfg(target_os = "macos")]
    {
        let supervisor = super::Supervisor::new(
            std::sync::Arc::new(crate::Registry::default()),
            super::RestartPolicy::default(),
        )
        .with_privacy_trampoline(std::env::current_exe().unwrap());
        let error = supervisor.spawn(spec()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("binary does not implement the privacy trampoline protocol"),
            "{error}"
        );
    }
}

#[cfg(target_os = "macos")]
fn spec() -> super::ModuleSpec {
    super::ModuleSpec {
        module_id: "privacy-configuration".into(),
        program: "/bin/sleep".into(),
        args: vec!["30".into()],
        env: vec![],
        reserved: false,
        reserved_prefixes: vec![],
        protocol: subc_control::ModuleProtocol::None,
        overlap: super::ModuleOverlap::Exclusive,
        carry_containment: false,
        script: None,
    }
}
