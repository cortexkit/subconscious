//! Connection-bound peer evidence. Initial recognition is shared by every
//! caller on a connection; revalidation checks a snapshot without holding locks.

use super::*;
use crate::operator_apps::{OperatorAppStamp, EVIDENCE_BUDGET};
use subc_os::local_peer::CapturedPeer;
use tokio::sync::watch;

#[derive(Debug)]
pub(super) struct PeerCapture {
    peer: CapturedPeer,
    recognized: std::sync::OnceLock<Recognition>,
}

impl PeerCapture {
    pub(super) fn observed_pid(&self) -> Option<u32> {
        match self.peer.observation() {
            subc_os::local_peer::ObservedPeer::Observed(peer) => Some(peer.pid),
            subc_os::local_peer::ObservedPeer::Unobserved => None,
        }
    }
}

#[derive(Debug)]
struct Recognition {
    deadline: std::time::Instant,
    verdict: watch::Receiver<RecognitionVerdict>,
}

#[derive(Debug)]
enum RecognitionVerdict {
    Pending,
    Complete(Option<OperatorAppStamp>),
}

impl From<CapturedPeer> for PeerCapture {
    fn from(peer: CapturedPeer) -> Self {
        Self {
            peer,
            recognized: Default::default(),
        }
    }
}

type StampMap = Mutex<HashMap<ConnectionId, OperatorAppStamp>>;

impl ControlHandler {
    pub(crate) fn record_captured_peer(&self, connection: ConnectionId, peer: CapturedPeer) {
        self.record_observed_peer(connection, peer.observation().clone());
        #[cfg(windows)]
        {
            if let Some(identity) = self.config_windows_launch_identity(&peer) {
                // Keep the connect-time incarnation; looking it up later from
                // a numeric pipe-client PID could match a replacement process.
                self.config_desktop_stamps
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(connection, identity);
            }
            if let Some(stamp) = self.stamp_windows_launch(&peer) {
                self.operator_apps
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .insert(connection, stamp);
            }
        }
        self.captured_peers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(connection, Arc::new(peer.into()));
    }

    /// A caller joining the initial verification can use that fresh verdict
    /// without submitting a second native check.
    async fn recognize_peer(&self, connection: ConnectionId) -> Option<OperatorAppStamp> {
        let capture = self
            .captured_peers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&connection)
            .cloned();
        let capture =
            capture.filter(|capture| self.operator_app_policy.may_recognize(&capture.peer))?;
        let mut started = false;
        let recognition = capture.recognized.get_or_init(|| {
            started = true;
            let deadline = std::time::Instant::now() + EVIDENCE_BUDGET;
            let (tx, verdict) = watch::channel(RecognitionVerdict::Pending);
            let handler = self.clone();
            let capture = capture.clone();
            // A cancelled caller must not allow the next caller to resubmit
            // the same native job. The single attempt owns its publication.
            tokio::spawn(async move {
                let mut verified = None;
                if let Some((stamp, observed)) = handler
                    .operator_app_policy
                    .recognize(capture.peer.clone(), deadline)
                    .await
                {
                    let captures = handler
                        .captured_peers
                        .lock()
                        .unwrap_or_else(|p| p.into_inner());
                    if std::time::Instant::now() < deadline
                        && captures
                            .get(&connection)
                            .is_some_and(|live| Arc::ptr_eq(live, &capture))
                    {
                        handler.record_observed_peer(connection, observed);
                        verified = Some(stamp.clone());
                        handler
                            .config_desktop_stamps
                            .lock()
                            .unwrap_or_else(|p| p.into_inner())
                            .insert(connection, stamp.clone());
                        if stamp.delivery_enabled(&handler.operator_app_policy) {
                            handler
                                .operator_apps
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .insert(connection, stamp);
                        }
                    }
                }
                tx.send_replace(RecognitionVerdict::Complete(verified));
            });
            Recognition { deadline, verdict }
        });
        let mut verdict = recognition.verdict.clone();
        let waiting = matches!(*verdict.borrow_and_update(), RecognitionVerdict::Pending);
        if waiting {
            tokio::time::timeout_at(recognition.deadline.into(), verdict.changed())
                .await
                .ok()?
                .ok()?;
        }
        if !started && !waiting {
            return None;
        }
        let result = match &*verdict.borrow() {
            RecognitionVerdict::Complete(stamp) => stamp.clone(),
            RecognitionVerdict::Pending => None,
        };
        result
    }

    async fn recognize_operator_peer(&self, connection: ConnectionId) -> Option<OperatorAppStamp> {
        if self.operator_app_policy.macos_enabled {
            self.recognize_peer(connection).await
        } else {
            None
        }
    }

    fn cached_stamp(stamps: &StampMap, connection: ConnectionId) -> Option<OperatorAppStamp> {
        stamps
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&connection)
            .cloned()
    }

    async fn current_stamp(
        &self,
        stamps: &StampMap,
        connection: ConnectionId,
        delivery_required: bool,
    ) -> Option<OperatorAppStamp> {
        let fresh = if delivery_required {
            self.recognize_operator_peer(connection).await
        } else {
            self.recognize_peer(connection).await
        };
        let stamp = Self::cached_stamp(stamps, connection)?;
        if delivery_required && !stamp.delivery_enabled(&self.operator_app_policy) {
            return None;
        }
        let freshly_verified = fresh.is_some_and(|fresh| fresh.same_generation(&stamp));
        if !freshly_verified && !stamp.revalidate_with_launches(&self.launch_table).await {
            return None;
        }
        Self::cached_stamp(stamps, connection).filter(|current| stamp.same_generation(current))
    }

    #[cfg(not(feature = "dev-desktop-approval"))]
    pub(super) async fn has_current_config_desktop_stamp(&self, connection: ConnectionId) -> bool {
        self.current_stamp(&self.config_desktop_stamps, connection, false)
            .await
            .is_some()
    }

    pub(super) async fn operator_shortcut_stamp(
        &self,
        connection: ConnectionId,
    ) -> Option<OperatorAppStamp> {
        self.current_stamp(&self.operator_apps, connection, true)
            .await
    }

    pub(super) async fn route_operator_app(
        &self,
        connection: ConnectionId,
        capabilities: Option<&CapabilityDeclarations>,
    ) -> Option<OperatorAppStamp> {
        if capabilities.is_some_and(|caps| {
            caps.provides
                .iter()
                .any(|cap| cap == OPERATOR_APP_CAPABILITY)
        }) {
            self.recognize_operator_peer(connection).await;
        }
        Self::cached_stamp(&self.operator_apps, connection)
    }

    pub(super) async fn handle_connection_describe(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        if serde_json::from_slice::<subc_control::ConnectionDescribeRequest>(&frame.body).is_err() {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_control_body",
                "malformed connection.describe body",
            )?]);
        }
        let local = self
            .local_connections
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(&ctx.connection_id);
        self.recognize_operator_peer(ctx.connection_id).await;
        let stamp = Self::cached_stamp(&self.operator_apps, ctx.connection_id);
        let operator_app = self.connection_operator_app(stamp.as_ref());
        Ok(vec![control_response_body_frame(
            &frame,
            &subc_control::ConnectionDescribeReply::new(local, operator_app),
            ops::CONNECTION_DESCRIBE,
        )?])
    }
}

impl ControlHandler {
    pub(super) async fn handle_operator_confirm(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
    ) -> Result<Vec<Frame>, RouterError> {
        use crate::operator_confirm::{audit, Outcome, ShortcutContext};
        let registration = self
            .registry
            .get_module_by_connection(ctx.connection_id)
            .map_err(|err| RouterError::backend(0, frame.header.corr, err.to_string()))?;
        let Some(registration) = registration else {
            let outcome = Outcome::refusal("not_registered");
            audit("", "", "", &outcome, Duration::ZERO, Duration::ZERO, false);
            return Ok(vec![outcome.frame(&frame)]);
        };
        let request = match serde_json::from_slice::<OperatorConfirmRequest>(&frame.body) {
            Ok(request) => request,
            Err(_) => {
                let outcome = Outcome::refusal("invalid_control_body");
                audit("", "", "", &outcome, Duration::ZERO, Duration::ZERO, false);
                return Ok(vec![outcome.frame(&frame)]);
            }
        };
        let declared = matches!(
            request.kind,
            Some(subc_protocol::session::OperatorConfirmKind::Operation)
        ) && request.operation.as_ref().is_some_and(|operation| {
            registration
                .manifest
                .confirm_operations
                .as_ref()
                .is_some_and(|operations| operations.contains(operation))
        });
        let module_id = registration.manifest.module_id;
        // Read launch state before taking the forwarding lock. A registration
        // channel proves its own spawn; otherwise, use the launch nonce.
        let nonce = self
            .hello_launch_nonces
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .nonce(ctx.connection_id)
            .map(str::to_owned);
        let nonce_proven = self
            .confirm_channel_proven(ctx.connection_id, &module_id)
            .unwrap_or_else(|| {
                nonce.as_deref().is_some_and(|nonce| {
                    self.supervisor
                        .spawned_consumer_authorized(&module_id, nonce)
                })
            });
        let route = (request.route_channel, request.route_epoch);
        let candidate = if nonce_proven
            && crate::operator_confirm::valid_summary(&request.summary)
            && declared
        {
            self.forwarding
                .with_operator_route(ctx.connection_id, route.0, route.1, |binding| {
                    binding.map(|binding| {
                        (
                            binding.flow.clone(),
                            binding.client_connection_id,
                            ShortcutContext::phone_fingerprint(&self.operator_app_policy, binding),
                        )
                    })
                })
                .map_err(RouterError::Forwarding)?
        } else {
            None
        };
        let shortcut = if let Some((flow, opener, phone)) = candidate {
            let stamp = if phone.is_none() {
                self.operator_shortcut_stamp(opener).await
            } else {
                None
            };
            (phone.is_some() || stamp.is_some()).then(|| {
                (
                    flow,
                    self.shortcut_context(stamp, phone, Some(route), opener),
                )
            })
        } else {
            None
        };
        let confirms = self.forwarding.operator_confirms();
        self.forwarding
            .with_operator_route(ctx.connection_id, route.0, route.1, |binding| {
                let shortcut = shortcut.and_then(|(flow, context)| {
                    binding
                        .filter(|current| {
                            Arc::ptr_eq(&current.flow, &flow)
                                && context.stamp.as_ref().is_none_or(|stamp| {
                                    current
                                        .operator_app
                                        .as_ref()
                                        .map_or(stamp.launch_incarnation.is_none(), |bound| {
                                            bound.same_generation(stamp)
                                        })
                                })
                        })
                        .map(|_| context)
                });
                confirms.admit_with_shortcut(
                    ctx,
                    frame,
                    module_id,
                    nonce_proven,
                    request,
                    binding,
                    shortcut,
                )
            })
            .map_err(RouterError::Forwarding)
    }

    pub(super) fn shortcut_context(
        &self,
        stamp: Option<OperatorAppStamp>,
        phone_fingerprint: Option<String>,
        route: Option<(u16, u32)>,
        stamp_owner: ConnectionId,
    ) -> crate::operator_confirm::ShortcutContext {
        crate::operator_confirm::ShortcutContext {
            ledger: self.activity.clone(),
            forwarding: self.forwarding.clone(),
            launches: self.launch_table.clone(),
            stamp,
            phone_fingerprint,
            route,
            connections: self.operator_apps.clone(),
            stamp_owner: Some(stamp_owner),
            #[cfg(test)]
            hooks: Default::default(),
        }
    }
}
