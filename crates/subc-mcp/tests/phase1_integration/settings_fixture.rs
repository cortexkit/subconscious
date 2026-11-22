use super::*;
use subc_client_rs::config::{
    ConfigGetReply, ConfigLayer, ConfigLayerSnapshot, ConfigStatus, ConfigView, LayerValue,
    ValidationContext,
};

// The embedded relay daemon has no project settings owner. Keep the relay and
// supervisor real, but provide deterministic daemon settings views on its socket.
pub(super) async fn serve(
    listener: TcpListener,
    backend: PathBuf,
    info: ConnectionInfo,
    projects: bool,
) -> Result<(), subc_daemon::ServerError> {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (front, _) = accepted.unwrap();
                let backend = backend.clone();
                let info = info.clone();
                connections.spawn(async move { let _ = connection(front, backend, info, projects).await; });
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

async fn connection(
    mut front: TcpStream,
    backend: PathBuf,
    info: ConnectionInfo,
    projects: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    authenticate_server(
        &mut front,
        &info.key,
        &info.daemon_id,
        &info.daemon_ver,
        READ_TIMEOUT,
    )
    .await?;
    let back = connect_control_client(&backend).await?;
    let (mut front_rx, front_tx) = front.into_split();
    let (mut back_rx, mut back_tx) = back.into_split();
    let front_tx = Arc::new(TokioMutex::new(front_tx));
    let daemon_tx = front_tx.clone();
    let settings = Arc::new(TokioMutex::new(ConnectionSettings::default()));
    let poll_settings = settings.clone();
    let poll_tx = front_tx.clone();
    let requests = async {
        while let Some(frame) = read_frame(&mut front_rx).await? {
            let mut settings = settings.lock().await;
            if frame.header.channel == 0 && frame.header.ty == FrameType::Request {
                let body: Value = serde_json::from_slice(&frame.body).unwrap_or(Value::Null);
                let reply = match body["op"].as_str() {
                    Some("config.declare") => {
                        settings.user = body["user_file"].as_str().map(PathBuf::from);
                        let schema = subc_client_rs::config::Schema::new(body["schema"].clone())
                            .map_err(|error| format!("invalid test gateway schema: {error:?}"))?;
                        settings.schema = Some(schema);
                        Some((FrameType::Response, json!({"op":"config.declare"})))
                    }
                    Some("config.get") => {
                        let project = body["project_id"].as_str().map(str::to_owned);
                        let view = settings.view(project.clone());
                        settings.last.insert(project, view.layers.clone());
                        Some(match settings.validate(&view) {
                            Ok(()) => (
                                FrameType::Response,
                                serde_json::to_value(ConfigGetReply::new(Some(view), None))?,
                            ),
                            Err(error) => (
                                FrameType::Error,
                                json!({"code":"value_invalid", "message":error.to_string()}),
                            ),
                        })
                    }
                    _ => None,
                };
                if let Some((ty, reply)) = reply {
                    let mut writer = front_tx.lock().await;
                    send(&mut *writer, &frame, ty, reply).await?;
                    continue;
                }
            }
            drop(settings);
            write_frame(&mut back_tx, &frame).await?;
            back_tx.flush().await?;
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    };
    let responses = async {
        while let Some(frame) = read_frame(&mut back_rx).await? {
            let mut writer = daemon_tx.lock().await;
            if frame.header.ty == FrameType::HelloAck {
                let mut body: Value = serde_json::from_slice(&frame.body)?;
                let capabilities = body["subc_capabilities"].as_array_mut().unwrap();
                capabilities.push(json!("config.gate"));
                send(&mut *writer, &frame, FrameType::HelloAck, body).await?;
            } else if frame.header.channel == 0
                && frame.header.ty == FrameType::Response
                && projects
            {
                let body = serde_json::from_slice::<Value>(&frame.body);
                if let Ok(mut body) = body {
                    if body["op"] == "server.describe" {
                        body["capabilities"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!("config.settings.projects/v1"));
                        send(&mut *writer, &frame, FrameType::Response, body).await?;
                        continue;
                    }
                }
                write_frame(&mut *writer, &frame).await?;
                writer.flush().await?;
            } else {
                write_frame(&mut *writer, &frame).await?;
                writer.flush().await?;
            }
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    };
    let notifications = async {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        let mut notified = BTreeMap::new();
        loop {
            interval.tick().await;
            let settings = poll_settings.lock().await;
            for (id, last) in &settings.last {
                let view = settings.view(id.clone());
                if view.layers == *last {
                    notified.remove(id);
                    continue;
                }
                let digests: Vec<_> = view
                    .layers
                    .iter()
                    .map(|layer| layer.digest.clone())
                    .collect();
                if notified.get(id) == Some(&digests) {
                    continue;
                }
                let mut body = json!({"op":"config.changed", "generation":0, "fields":[]});
                if let Some(id) = id {
                    body["project_id"] = json!(id);
                }
                let frame = Frame::build(
                    FrameType::Push,
                    Flags::new(false, Priority::Passive, false),
                    0,
                    0,
                    0,
                    serde_json::to_vec(&body)?,
                )?;
                let mut writer = poll_tx.lock().await;
                write_frame(&mut *writer, &frame).await?;
                writer.flush().await?;
                notified.insert(id.clone(), digests);
            }
        }
        #[allow(unreachable_code)]
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    };
    tokio::select! { result = requests => result, result = responses => result, result = notifications => result }
}

#[derive(Default)]
struct ConnectionSettings {
    user: Option<PathBuf>,
    schema: Option<subc_client_rs::config::Schema>,
    last: BTreeMap<Option<String>, Vec<ConfigLayerSnapshot>>,
}

impl ConnectionSettings {
    fn validate(&self, view: &ConfigView) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut layers = Vec::new();
        for layer in &view.layers {
            if let Some(bytes) = &layer.bytes {
                let document = subc_jsonc::jsonc_to_json(std::str::from_utf8(bytes)?)?;
                layers.push(LayerValue::new(
                    layer.layer.known_layer().unwrap(),
                    serde_json::from_str(&document)?,
                ));
            }
        }
        self.schema
            .as_ref()
            .expect("declaration precedes config.get")
            .review_tuple(
                &layers,
                &ValidationContext::new(TEST_MCP_MODULE_ID, view.project_id.clone()),
            )
            .map_err(|error| format!("settings validation failed: {error:?}"))?;
        Ok(())
    }

    fn view(&self, project: Option<String>) -> ConfigView {
        let mut layers = Vec::new();
        if let Some(path) = &self.user {
            layers.push(ConfigLayerSnapshot::file(
                TEST_MCP_MODULE_ID,
                ConfigLayer::User,
                path.to_string_lossy(),
                fs::read(path).ok(),
            ));
        }
        if let Some(id) = &project {
            let path = Path::new(id).join(".cortexkit/subc-mcp.jsonc");
            let mut layer = ConfigLayerSnapshot::file(
                TEST_MCP_MODULE_ID,
                ConfigLayer::Project,
                path.to_string_lossy(),
                fs::read(&path).ok(),
            );
            layer.project_id = Some(id.clone());
            layers.push(layer);
        }
        // Deliberately do not supply flattened values. The gateway must use
        // the daemon's individual layer documents to preserve narrowing.
        let mut view = ConfigView::new(
            0,
            json!({}),
            BTreeMap::new(),
            layers,
            ConfigStatus::Unstamped,
        );
        view.project_id = project;
        view
    }
}

async fn send<W: AsyncWrite + Unpin>(
    writer: &mut W,
    request: &Frame,
    ty: FrameType,
    value: Value,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let frame = Frame::build_with_version(
        request.header.ver,
        ty,
        request.header.flags,
        request.header.channel,
        request.header.epoch,
        request.header.corr,
        serde_json::to_vec(&value)?,
    )?;
    write_frame(writer, &frame).await?;
    writer.flush().await?;
    Ok(())
}
