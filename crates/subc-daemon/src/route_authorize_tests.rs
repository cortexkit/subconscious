use super::*;
use subc_protocol::scope::{RouteAuthorizeExpected, RouteAuthorizeScope, ROUTE_AUTHORIZE_CAPABILITY};

#[tokio::test]
async fn route_authorize_permission_and_reply_are_connection_bound_and_side_effect_free() {
    for (sender, capability, permitted) in [("gateway", true, true), ("gateway", false, false), ("other-gateway", true, false)] {
        let mut rig = rig().await;
        rig.handler = rig.handler.clone().with_federated_scope_owner("gateway".into());
        let mut source = session(1);
        source.carriers.push(ScopeCarrier::new(Principal::Direct));
        rig.sync(vec![source.clone()]).await;
        let (ctx, mut rx) = wide_ctx(30);
        let hello = hello_with_capabilities(sender, 30, if capability { &[ROUTE_AUTHORIZE_CAPABILITY] } else { &[] });
        let hello_body: Value = serde_json::from_slice(&hello.body).unwrap();
        let reply = rig.handler.handle_control_frame(&ctx, hello).await.unwrap();
        assert!(reply.is_empty());
        let ack = rx.recv().await.unwrap().frame;
        assert_eq!(ack.header.ty, FrameType::HelloAck, "{hello_body}");
        let ack_body: Value = serde_json::from_slice(&ack.body).unwrap();
        assert!(ack_body["subc_ops"].as_array().unwrap().iter().any(|op| op == "route.authorize"));
        let request = ModuleControlRequestFromModule::RouteAuthorize {
            principal: Principal::Direct,
            scope: RouteAuthorizeScope::new(Principal::Reserved { module_id: OWNER.into() }, "s", 1),
            target: PLEXUS.into(), expected: RouteAuthorizeExpected::new(source.kind, &source.attributes),
        };
        let reply = call(&rig.handler, &ctx, &request).await;
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        if permitted {
            assert_eq!(reply.header.ty, FrameType::Response);
            assert_eq!(body["op"], "route.authorize");
            assert_eq!(body["outcome"], "authorized");
            assert_eq!(body["content_version"].as_str().unwrap().len(), 71);
            assert!(body.get("route_open_code").is_none());
            // The authorized principal carries no spawn or epoch, so the
            // reply never conveys an attestation or the first-party mark.
            for key in ["caller", "target", "first_party"] {
                assert!(body.get(key).is_none(), "{key}: {body}");
            }
            let (direct, _rx) = wide_ctx(31);
            let denied = call(&rig.handler, &direct, &request).await;
            assert_eq!(parse_error(&denied)["code"], error_codes::ROUTE_AUTHORIZE_NOT_PERMITTED);
        } else {
            assert_eq!(body["code"], error_codes::ROUTE_AUTHORIZE_NOT_PERMITTED);
        }
        assert_eq!(rig.forwarding.reserved_route_count().unwrap(), (0, 0));
        assert!(rig.modules.get_mut(PLEXUS).unwrap().1.try_recv().is_err());
        if permitted {
            let route = rig.bound(None, PLEXUS, Some(rig_selector("s", Some(1)))).await;
            assert!(rig.live(&route), "an authorized shared admission can open and bind");
        }
    }
}
