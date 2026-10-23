use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AttributionMode {
    Normal,
    NoWorkdir,
    RefuseScopeApply,
    StaleScopeNonce,
}

pub(super) struct AttributionProxy {
    pub(super) connection_file: PathBuf,
    pub(super) scope_requests: Arc<AtomicUsize>,
    pub(super) describes: Arc<AtomicUsize>,
    pub(super) errors: Arc<TokioMutex<Vec<Value>>>,
    task: JoinHandle<()>,
}

impl Drop for AttributionProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl AttributionProxy {
    pub(super) async fn start(server: &TestServer, mode: AttributionMode) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let connection_file = server.daemon.temp_dir.join("attribution-proxy.json");
        let key = generate_key().unwrap();
        let daemon_id = generate_daemon_id().unwrap();
        let info = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: None,
            endpoints: vec![Endpoint {
                host: Ipv4Addr::LOCALHOST.to_string(),
                port: listener.local_addr().unwrap().port(),
            }],
            key: key.clone(),
            daemon_id,
            pid: process::id(),
            daemon_ver: "attribution-test-proxy".into(),
        };
        write_atomic(&connection_file, &info).unwrap();
        let real_connection = server.daemon.connection_file_path.clone();
        let scope_requests = Arc::new(AtomicUsize::new(0));
        let describes = Arc::new(AtomicUsize::new(0));
        let errors = Arc::new(TokioMutex::new(Vec::new()));
        let scope_count = scope_requests.clone();
        let describe_count = describes.clone();
        let received_errors = errors.clone();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (mut front, _) = accepted.unwrap();
                        let key = key.clone();
                        let real_connection = real_connection.clone();
                        let scope_count = scope_count.clone();
                        let describe_count = describe_count.clone();
                        let received_errors = received_errors.clone();
                        connections.spawn(async move {
                            authenticate_server(&mut front, &key, &daemon_id, "attribution-test-proxy", READ_TIMEOUT).await.unwrap();
                            let back = connect_control_client(&real_connection).await.unwrap();
                            let (mut front_read, front_write) = front.into_split();
                            let (mut back_read, mut back_write) = back.into_split();
                            let front_write = Arc::new(TokioMutex::new(front_write));
                            let request_write = front_write.clone();
                            let requests = async move {
                                while let Some(mut frame) = read_frame(&mut front_read).await.unwrap() {
                                    if mode == AttributionMode::StaleScopeNonce && frame.header.ty == FrameType::Hello {
                                        let mut hello: Value = serde_json::from_slice(&frame.body).unwrap();
                                        hello["launch_nonce"] = json!("not-the-current-launch-nonce");
                                        frame = Frame::build(FrameType::Hello, frame.header.flags, 0, 0, frame.header.corr, serde_json::to_vec(&hello).unwrap()).unwrap();
                                    }
                                    let body = serde_json::from_slice::<Value>(&frame.body).unwrap_or(Value::Null);
                                    let op = body.get("op").and_then(Value::as_str);
                                    if frame.header.channel == 0 && frame.header.ty == FrameType::Request {
                                        if op == Some("server.describe") {
                                            describe_count.fetch_add(1, Ordering::SeqCst);
                                        }
                                        if matches!(op, Some("scope.sync" | "scope.apply")) {
                                            scope_count.fetch_add(1, Ordering::SeqCst);
                                        }
                                        if op == Some("scope.apply") && mode == AttributionMode::RefuseScopeApply {
                                            let refusal = Frame::build(FrameType::Error, frame.header.flags, 0, 0, frame.header.corr,
                                                serde_json::to_vec(&json!({"code":"scope_forbidden", "message":"scope registration refused by test policy"})).unwrap()).unwrap();
                                            let mut writer = request_write.lock().await;
                                            write_frame(&mut *writer, &refusal).await.unwrap();
                                            writer.flush().await.unwrap();
                                            continue;
                                        }
                                    }
                                    write_frame(&mut back_write, &frame).await.unwrap();
                                    back_write.flush().await.unwrap();
                                }
                            };
                            let responses = async move {
                                while let Some(frame) = read_frame(&mut back_read).await.unwrap() {
                                    let mut body = serde_json::from_slice::<Value>(&frame.body).unwrap_or(Value::Null);
                                    if frame.header.ty == FrameType::Error {
                                        received_errors.lock().await.push(body.clone());
                                    }
                                    let frame = if mode == AttributionMode::NoWorkdir && body["op"] == "server.describe" {
                                        let capabilities = body.get_mut("capabilities").and_then(Value::as_array_mut).expect("server capabilities");
                                        capabilities.retain(|value| value != "scope-workdir/v1");
                                        Frame::build(frame.header.ty, frame.header.flags, frame.header.channel, frame.header.epoch, frame.header.corr, serde_json::to_vec(&body).unwrap()).unwrap()
                                    } else { frame };
                                    let mut writer = front_write.lock().await;
                                    write_frame(&mut *writer, &frame).await.unwrap();
                                    writer.flush().await.unwrap();
                                }
                            };
                            tokio::select! { _ = requests => {}, _ = responses => {} }
                        });
                    }
                    completed = connections.join_next(), if !connections.is_empty() => {
                        completed.unwrap().unwrap();
                    }
                }
            }
        });
        Self {
            connection_file,
            scope_requests,
            describes,
            errors,
            task,
        }
    }
}
