use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub struct Proxy {
    frames: Arc<Mutex<Vec<Value>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Proxy {
    pub async fn start(root: &Path) -> Self {
        let connection = root.join("runtime/subc-connection.json");
        let mut info: Value = serde_json::from_slice(&fs::read(connection).unwrap()).unwrap();
        let host = info["endpoints"][0]["host"].as_str().unwrap().to_string();
        let port = info["endpoints"][0]["port"].as_u64().unwrap() as u16;
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        info["endpoints"][0]["host"] = json!("127.0.0.1");
        info["endpoints"][0]["port"] = json!(listener.local_addr().unwrap().port());
        let path = root.join("proxy-connection.json");
        fs::write(&path, serde_json::to_vec(&info).unwrap()).unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        let frames = Arc::new(Mutex::new(Vec::new()));
        let log = frames.clone();
        let root = root.to_path_buf();
        let task = tokio::spawn(async move {
            while let Ok((module, _)) = listener.accept().await {
                let daemon = TcpStream::connect((host.as_str(), port)).await.unwrap();
                let (mr, mw) = module.into_split();
                let (dr, dw) = daemon.into_split();
                let sent = log.clone();
                let received = log.clone();
                let sent_root = root.clone();
                let received_root = root.clone();
                tokio::spawn(async move {
                    tokio::join!(
                        relay(mr, dw, sent, "module", sent_root),
                        relay(dr, mw, received, "daemon", received_root)
                    );
                });
            }
        });
        Self { frames, task }
    }
    pub fn frames(&self) -> Vec<Value> {
        self.frames.lock().unwrap().clone()
    }
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// Observe real frames without inventing responses or altering refusal codes.
// Authentication frames are forwarded but never retained in the event log.
async fn relay(
    mut reader: tokio::net::tcp::OwnedReadHalf,
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    log: Arc<Mutex<Vec<Value>>>,
    direction: &str,
    root: PathBuf,
) {
    for _ in 0..if direction == "module" { 2 } else { 1 } {
        let mut length = [0; 4];
        if reader.read_exact(&mut length).await.is_err() {
            return;
        }
        let mut body = vec![0; u32::from_le_bytes(length) as usize];
        if reader.read_exact(&mut body).await.is_err() {
            return;
        }
        if writer.write_all(&length).await.is_err() || writer.write_all(&body).await.is_err() {
            return;
        }
    }
    while let Ok(Some(frame)) = read_frame(&mut reader).await {
        if let Ok(body) = serde_json::from_slice::<Value>(&frame.body) {
            if body["op"]
                .as_str()
                .is_some_and(|op| op.starts_with("config."))
                || body["code"].as_str().is_some_and(|code| {
                    matches!(
                        code,
                        "config_gated" | "anchor_pending" | "not_delivery_target"
                    )
                })
            {
                log.lock().unwrap().push(json!({"direction":direction,"corr":frame.header.corr,"type":format!("{:?}",frame.header.ty),"body":body}));
            }
            if direction == "module" && body["op"] == "config.declare" {
                if let Some(module) = body["user_file"]
                    .as_str()
                    .and_then(|path| Path::new(path).file_stem())
                    .and_then(|name| name.to_str())
                {
                    let hold = root.join(format!("hold-declare-{module}"));
                    while hold.exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
        }
        if write_frame(&mut writer, &frame).await.is_err() {
            break;
        }
    }
}
