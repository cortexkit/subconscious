use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
};

use serde_json::{json, Value};

fn main() {
    let mode = std::env::var("FIXTURE_MODE").unwrap_or_else(|_| "normal".to_string());
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();

    #[cfg(unix)]
    let mut grandchild = None;
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = request.get("method").and_then(Value::as_str);
        if method.is_none() {
            continue;
        }
        if method == Some("notifications/initialized") {
            continue;
        }
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        if method == Some("initialize") {
            if mode == "initialize-error" {
                write_frame(
                    &mut stdout,
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"invalid"}}),
                );
                continue;
            }
            write_frame(
                &mut stdout,
                json!({"jsonrpc":"2.0","id":id,"result":{"capabilities":{}}}),
            );
            continue;
        }
        if mode == "hang" {
            // Simulate a wedged child: the request is consumed from stdin but
            // no response frame is ever written, while the process stays alive
            // and keeps its pipes open.
            continue;
        }
        if mode == "early-exit" {
            return;
        }
        if mode == "slow" {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        if mode == "oversized" {
            write_frame(
                &mut stdout,
                json!({"jsonrpc":"2.0","id":id,"result":{"bytes":"x".repeat(256)}}),
            );
            continue;
        }

        if mode == "same-id-ping" {
            write_frame(
                &mut stdout,
                json!({"jsonrpc":"2.0","id":id,"method":"ping"}),
            );
        }

        let environment: BTreeMap<_, _> = std::env::vars().collect();
        #[cfg(unix)]
        if mode == "tree" && grandchild.is_none() {
            let mut helper = std::process::Command::new("/bin/sh")
                .args(["-c", "trap '' TERM; echo ready; exec sleep 600"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let mut ready = String::new();
            io::BufReader::new(helper.stdout.take().unwrap())
                .read_line(&mut ready)
                .unwrap();
            assert_eq!(ready.trim(), "ready");
            write_frame(
                &mut stdout,
                json!({"jsonrpc":"2.0","id":id,"result":{"grandchild_pid":helper.id()}}),
            );
            grandchild = Some(helper);
            continue;
        }
        let result = match method {
            Some("tools/list") if mode == "paginated" => {
                if request.pointer("/params/cursor").and_then(Value::as_str) == Some("p2") {
                    json!({"tools": [{"name": "second"}]})
                } else {
                    json!({"tools": [{"name": "first"}], "nextCursor": "p2"})
                }
            }
            Some("tools/list") => json!({"tools": [{"name": "fixture"}]}),
            Some("tools/call") => json!({
                "echo": request.get("params").cloned().unwrap_or(Value::Null),
                "environment": environment,
                "pid": std::process::id(),
            }),
            _ => json!({"unexpected_method": method}),
        };
        write_frame(
            &mut stdout,
            json!({"jsonrpc":"2.0","id":id,"result":result}),
        );
    }
    #[cfg(unix)]
    if let Some(mut helper) = grandchild {
        let _ = helper.wait();
    }
}

fn write_frame(stdout: &mut impl Write, value: Value) {
    serde_json::to_writer(&mut *stdout, &value).expect("fixture response serializes");
    stdout
        .write_all(b"\n")
        .expect("fixture stdout is available");
    stdout.flush().expect("fixture stdout flushes");
}
