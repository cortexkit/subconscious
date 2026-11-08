use super::*;
use serde_json::{json, Value};

fn frame(body: Value) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        91,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn config_gate_ops_are_explicitly_unprivileged_and_inactive_dispatch_is_named() {
    let handler = ControlHandler::new(Arc::new(Registry::default()));
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let ctx = RouteCtx {
        connection_id: ConnectionId::new(11),
        egress: crate::FrameSink::new(tx),
    };
    for body in [
        json!({"op":"config.status"}),
        json!({"op":"config.plan"}),
        json!({"op":"config.apply","digest":subc_config::raw_digest(b"plan")}),
        json!({"op":"config.stamp"}),
        json!({"op":"config.unstamp"}),
        json!({"op":"config.history"}),
        json!({"op":"config.rollback","generation":1}),
    ] {
        let request: ConfigControlRequest = serde_json::from_value(body.clone()).unwrap();
        assert!(privileged_operation(&request).is_none(), "{}", request.op());
        assert!(SUBC_UNADVERTISED_CONTROL_OPS.contains(&request.op()));
        let response = handler
            .handle_control_frame(&ctx, frame(body))
            .await
            .unwrap();
        assert_eq!(response.len(), 1);
        assert_eq!(response[0].header.ty, FrameType::Error);
        assert_eq!(
            serde_json::from_slice::<Value>(&response[0].body).unwrap()["code"],
            "config_gate_inactive"
        );
    }
    for body in [
        json!({"op":"config.apply"}),
        json!({"op":"config.plan","reviewed":true}),
    ] {
        let response = handler
            .handle_control_frame(&ctx, frame(body))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&response[0].body).unwrap()["code"],
            "invalid_control_body"
        );
    }
}
