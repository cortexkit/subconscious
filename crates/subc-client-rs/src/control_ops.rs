use super::*;

pub(crate) fn validate(ops: Vec<String>) -> Result<Vec<String>, SubcModuleError> {
    let mut seen = std::collections::HashSet::new();
    for op in &ops {
        let valid = !op.is_empty()
            && op
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b))
            && op.split(['.', '/']).all(|segment| !segment.is_empty());
        if !valid
            || matches!(op.as_str(), "health.check" | "config.review" | "route.bind")
            || !seen.insert(op)
        {
            return Err(SubcModuleError::InvalidControlOp(op.clone()));
        }
    }
    Ok(ops)
}

pub(crate) async fn request<H: ModuleHandler>(
    frame: &Frame,
    egress: &mpsc::Sender<Frame>,
    handler: Arc<H>,
    dispatcher: &RequestDispatcher,
    handle: &ModuleHandle,
) -> Result<bool, SubcModuleError> {
    let shared = handle.shared();
    let Some(ops) = shared.served_control_ops.get() else {
        return Ok(false);
    };
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(&frame.body) else {
        return Ok(false);
    };
    let Some(op) = body
        .get("op")
        .or_else(|| body.get("protocol"))
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(false);
    };
    if !ops.iter().any(|declared| declared == op) {
        return Ok(false);
    }
    let permit = match dispatcher.control_slots.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            let reply = reply(
                frame,
                Err(ErrorBody::new(
                    "resource_exhausted",
                    "control handler slots are full",
                )),
            )?;
            send_outbound(egress, reply).await?;
            return Ok(true);
        }
    };
    let key = (frame.header.channel, frame.header.epoch, frame.header.corr);
    let cancellation = CancellationToken::new();
    {
        let mut pending = lock_in_flight(&dispatcher.in_flight)?;
        if pending.contains_key(&key) {
            return Err(SubcModuleError::DuplicateControlCorrelation(
                frame.header.corr,
            ));
        }
        pending.insert(key, cancellation.clone());
    }
    let op = op.to_owned();
    let frame = frame.clone();
    let egress = egress.clone();
    let state = dispatcher.clone();
    dispatcher.spawn(async move {
        let _permit = permit;
        let result = async {
            let outcome = catch_handler_panic(async {
                handler
                    .on_control_request(&op, body, cancellation.clone())
                    .await
            })
            .await;
            let panicked = outcome.is_err();
            let outcome = outcome
                .unwrap_or_else(|()| Err(ErrorBody::new("internal", "module handler panicked")));
            if !cancellation.is_cancelled() {
                let reply = reply(&frame, outcome)?;
                // Reserve outside the registry lock: a full writer queue must
                // not prevent the reader from delivering CANCEL. Commit under
                // the same lock as CANCEL so a cancelled reply cannot slip in.
                let slot = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Ok(()),
                    slot = egress.reserve() => slot.map_err(|_| SubcModuleError::WriterClosed)?,
                };
                let _pending = lock_in_flight(&state.in_flight)?;
                if !cancellation.is_cancelled() {
                    slot.send(reply);
                }
            }
            if panicked {
                return Err(SubcModuleError::HandlerPanic);
            }
            Ok(())
        }
        .await;
        if let Ok(mut pending) = state.in_flight.lock() {
            pending.remove(&key);
        }
        if let Err(error) = result {
            state.fail(error);
        }
    });
    Ok(true)
}

fn reply(
    request: &Frame,
    outcome: Result<serde_json::Value, ErrorBody>,
) -> Result<Frame, SubcModuleError> {
    let (ty, body) = match outcome {
        Ok(body) => (FrameType::Response, serde_json::to_vec(&body)?),
        Err(error) => (FrameType::Error, serde_json::to_vec(&error)?),
    };
    let (ty, flags, body) = bounded_terminal(ty, control_flags(), body)?;
    Frame::build_with_version(
        request.header.ver,
        ty,
        flags,
        0,
        0,
        request.header.corr,
        body,
    )
    .map_err(SubcModuleError::FrameBuild)
}

#[cfg(test)]
mod tests;
