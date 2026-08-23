use std::{
    collections::BTreeMap,
    io::{self, BufRead, Write},
};

use serde_json::{json, Value};

fn main() {
    let mode = std::env::var("FIXTURE_MODE").unwrap_or_else(|_| "normal".to_string());
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = request.get("method").and_then(Value::as_str);
        if method == Some("notifications/initialized") {
            continue;
        }
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        if method == Some("initialize") {
            write_frame(
                &mut stdout,
                json!({"jsonrpc":"2.0","id":id,"result":{"capabilities":{}}}),
            );
            continue;
        }
        if mode == "oversized" {
            write_frame(
                &mut stdout,
                json!({"jsonrpc":"2.0","id":id,"result":{"bytes":"x".repeat(256)}}),
            );
            continue;
        }

        let environment: BTreeMap<_, _> = std::env::vars().collect();
        let result = match method {
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
}

fn write_frame(stdout: &mut impl Write, value: Value) {
    serde_json::to_writer(&mut *stdout, &value).expect("fixture response serializes");
    stdout
        .write_all(b"\n")
        .expect("fixture stdout is available");
    stdout.flush().expect("fixture stdout flushes");
}
