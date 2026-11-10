use super::*;
use subc_client_rs::{
    async_trait, config::*, serve_with_handle, HandlerOutcome, HealthReport, ModuleHandler,
    RequestCtx,
};
use subc_protocol::manifest::ModuleManifest;

struct Counter {
    root: PathBuf,
    module: String,
    current: Mutex<Option<ConfigView>>,
}
impl Counter {
    fn event(&self, value: Value) {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("module-events"))
            .unwrap();
        let mut value = value;
        value["module"] = json!(self.module);
        file.write_all(format!("{value}\n").as_bytes()).unwrap();
    }
}

#[async_trait]
impl ModuleHandler for Counter {
    async fn handle(&self, _: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }
    fn config_declaration(&self) -> Option<ConfigDeclareRequest> {
        Some(ConfigDeclareRequest::new(
            json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema",
                "unevaluatedProperties":false,
                "type":"object", "properties":{"count":{"type":"integer", "description":"Counter value", "default":0,
                    "x-ck-apply":"live", "x-ck-layers":["user"]}}
            }),
            self.root
                .join(format!("config/cortexkit/{}.jsonc", self.module))
                .to_str()
                .unwrap(),
            vec![],
        ))
    }
    async fn config_review(&self, request: ConfigReviewRequest) -> ConfigReviewReply {
        let count = request
            .layers
            .iter()
            .filter_map(|layer| layer.bytes.as_deref())
            .filter_map(|bytes| serde_json::from_slice::<Value>(bytes).ok())
            .find_map(|value| value["count"].as_i64())
            .unwrap_or(0);
        let old = request
            .base
            .as_ref()
            .map(|base| base.effective["count"].clone())
            .unwrap_or(Value::Null);
        self.event(json!({"state":"review","count":count,"request":request}));
        if old == json!(count)
            && request
                .base
                .as_ref()
                .is_some_and(|base| base.sources.get("/count") == Some(&Layer::User))
        {
            return ConfigReviewReply::new(vec![], vec![]);
        }
        serde_json::from_value(json!({"op":"config.review","refusals":[],"fields":[{
            "module_id":self.module,"project_id":null,"path":"/count",
            "old":{"kind":"value","value":old},"new":{"kind":"value","value":count},
            "old_source":request.base.as_ref().and_then(|base| base.sources.get("/count")).map(|source| serde_json::to_value(source).unwrap()).unwrap_or(json!("default")),
            "new_source":"user","apply":"live","reasons":["Counter value"]
        }]})).unwrap()
    }
    async fn config_apply(&self, view: ConfigView) -> Result<(), Vec<String>> {
        let count = view.effective["count"].as_i64().unwrap();
        self.event(json!({"state":"applying","count":count,"view":view,"pid":std::process::id()}));
        let hold = self.root.join(format!("hold-{count}"));
        while hold.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if count < 0 {
            return Err(vec!["negative count rejected by the module".into()]);
        }
        *self.current.lock().unwrap() = Some(view);
        Ok(())
    }
    async fn on_config_state(&self, state: ConfigState) {
        let refusal = match &state {
            ConfigState::ConfigGated { refusal, .. }
            | ConfigState::NotDeliveryTarget { refusal, .. }
            | ConfigState::AnchorPending { refusal, .. } => Some(refusal),
            _ => None,
        };
        let (availability, view) = match &state {
            ConfigState::ConfigGated { view, .. }
            | ConfigState::NotDeliveryTarget { view, .. }
            | ConfigState::AnchorPending { view, .. }
            | ConfigState::Failed { view, .. } => match view {
                ConfigAvailability::NoApprovedConfig => ("no_approved_config", None),
                ConfigAvailability::LastView(view) => ("last_view", Some(view.clone())),
                _ => ("unknown", None),
            },
            ConfigState::Unstamped(view)
            | ConfigState::Pending(view)
            | ConfigState::Active(view) => ("view", Some(view.clone())),
            _ => ("unknown", None),
        };
        let current = self
            .current
            .lock()
            .unwrap()
            .as_ref()
            .map(|view| view.effective.clone());
        self.event(json!({"state":state.as_str(),"refusal":refusal,"availability":availability,"count":view.as_ref().map(|v| &v.effective["count"]),"view":view,"current":current,"pid":std::process::id()}));
    }
    async fn config_files(&self) {
        panic!("gate refusal must never read local files");
    }
    async fn on_config_error(&self, error: ConfigError) {
        self.event(json!({"state":"error","error":error.to_string()}));
    }
    async fn health(&self) -> HealthReport {
        while self.root.join("hold-health").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if self.root.join("observe-health").exists() {
            self.event(json!({"state":"health"}));
        }
        HealthReport::ok()
    }
    async fn on_draining(&self, _: subc_client_rs::RouteCloseReason, _: std::time::SystemTime) {
        self.event(json!({"state":"draining"}));
    }
    async fn on_connection_end(&self, _: subc_client_rs::ConnectionEnd) {
        self.event(json!({"state":"connection_end"}));
    }
}

pub async fn rust_module(root: PathBuf) {
    let module = std::env::var("CONFIG_SDK_MODULE_ID").unwrap_or_else(|_| "counter".into());
    let counter = Counter {
        root: root.clone(),
        module: module.clone(),
        current: Mutex::new(None),
    };
    wait_for(|| root.join("proxy-connection.json").exists()).await;
    let (handle, serving) = serve_with_handle(
        &root.join("proxy-connection.json"),
        ModuleManifest::builder(module, "1.0.0").build(),
        counter,
    )
    .await
    .unwrap();
    assert!(handle.daemon_supports(CONFIG_GATE_CAPABILITY));
    serving.await.unwrap();
}
