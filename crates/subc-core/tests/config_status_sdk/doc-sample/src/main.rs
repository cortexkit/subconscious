include!(concat!(env!("OUT_DIR"), "/sample.rs"));
const _: () = assert!(ADOPTION_LINES <= 60, "documented adoption exceeds 60 lines");

// Delegate to the exact documented hooks; logging belongs to the harness, not
// to the adoption sample or its pure review function.
struct Observed(Counter, std::path::PathBuf);

#[async_trait]
impl ModuleHandler for Observed {
    async fn handle(&self, ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        self.0.handle(ctx, body).await
    }
    fn config_declaration(&self) -> Option<ConfigDeclareRequest> {
        self.0.config_declaration()
    }
    async fn config_review(&self, request: ConfigReviewRequest) -> ConfigReviewReply {
        self.0.config_review(request).await
    }
    async fn config_apply(&self, view: ConfigView) -> Result<(), Vec<String>> {
        self.0.config_apply(view).await
    }
    async fn config_files(&self) {
        panic!("a gate-capable daemon must not use the sample's file fallback");
    }
    async fn on_config_state(&self, state: ConfigState) {
        use std::io::Write;
        self.0.on_config_state(state.clone()).await;
        let generation = match &state {
            ConfigState::Unstamped(view)
            | ConfigState::Pending(view)
            | ConfigState::Active(view) => Some(view.generation),
            _ => None,
        };
        let event = json!({"state":state.as_str(),"generation":generation,"current":*self.0.value.lock().unwrap()});
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.1.join("module-events"))
            .unwrap()
            .write_all(format!("{event}\n").as_bytes())
            .unwrap();
    }
    async fn on_config_error(&self, error: ConfigError) {
        panic!("documented sample was refused by the real daemon: {error}");
    }
}

#[tokio::main]
async fn main() {
    let Some(root) = std::env::var_os("CONFIG_SDK_MODULE_ROOT") else {
        println!("{{\"adoption_lines\":{ADOPTION_LINES}}}");
        return;
    };
    let root = std::path::PathBuf::from(root);
    while !root.join("proxy-connection.json").exists() {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let counter = Counter {
        user_file: root
            .join("config/cortexkit/counter.jsonc")
            .to_string_lossy()
            .into_owned(),
        value: Mutex::new(json!(null)),
    };
    let (handle, serving) = subc_client_rs::serve_with_handle(
        &root.join("proxy-connection.json"),
        subc_protocol::manifest::ModuleManifest::builder("counter", "1.0.0").build(),
        Observed(counter, root),
    )
    .await
    .unwrap();
    assert!(handle.daemon_supports(CONFIG_GATE_CAPABILITY));
    serving.await.unwrap();
}
