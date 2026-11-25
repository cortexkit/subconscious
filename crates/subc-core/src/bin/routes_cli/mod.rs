use super::{
    display_field, format_duration, humanize_identifier, modules_array, print_table,
    terminal_safe_string, CkClient, CkError, ClientControlRequest, JsonOutput, Value,
};
use std::time::Duration;

pub(super) async fn supervisor_routes(
    client: &mut CkClient,
    module_id: Option<&str>,
    json_output: JsonOutput,
) -> Result<(), CkError> {
    let response = client
        .rpc_value(ClientControlRequest::SupervisorRoutes {
            module_id: module_id.map(str::to_owned),
        })
        .await?;
    if json_output.enabled {
        json_output.print(&response)?;
        return Ok(());
    }

    let mut rows = Vec::new();
    for module in modules_array(&response) {
        let module_id = display_field(module, "module_id");
        for route in module
            .get("routes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let consumer = match route.get("consumer").and_then(Value::as_object) {
                Some(consumer)
                    if consumer.get("kind").and_then(Value::as_str) == Some("reserved") =>
                {
                    consumer
                        .get("module_id")
                        .and_then(Value::as_str)
                        .unwrap_or("reserved")
                        .to_string()
                }
                Some(consumer) => format!(
                    "direct ({})",
                    consumer
                        .get("connection_id")
                        .and_then(Value::as_u64)
                        .map(|id| format!("connection {id}"))
                        .unwrap_or_else(|| "connection unknown".to_string())
                ),
                None => "direct (connection unknown)".to_string(),
            };
            let age = route
                .get("age_ms")
                .and_then(Value::as_u64)
                .map(|age| format_duration(Duration::from_millis(age)))
                .unwrap_or_else(|| "-".to_string());
            let state = if route
                .get("draining")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                match route.get("drain_reason").and_then(Value::as_str) {
                    Some(reason) => format!("draining ({})", humanize_identifier(reason)),
                    None => "draining".to_string(),
                }
            } else {
                "live".to_string()
            };
            rows.push(vec![
                module_id.clone(),
                consumer,
                age,
                state,
                route_opener_label(route),
            ]);
        }
    }

    if rows.is_empty() {
        println!("no live routes");
    } else {
        print_table(&["module", "consumer", "age", "state", "opener"], rows);
    }
    Ok(())
}

fn route_opener_label(route: &Value) -> String {
    let opener = &route["opener"];
    let Some(transport) = opener["transport"].as_str() else {
        return "-".to_string();
    };
    let mut label = terminal_safe_string(transport);
    if let Some(pid) = opener["pid"].as_u64() {
        label.push_str(&format!(" pid {pid}"));
        if let Some(command) = opener["command"].as_str() {
            label.push_str(&format!(" ({})", terminal_safe_string(command)));
        }
    }
    label
}
