//! Activity ledger helpers and refusal classification.

use std::{error::Error, fmt};

pub use subc_control::{
    ActivityActor, ActivityCursor, ActivityEntry, ActivityOn, ActivityQueryRequest,
    ActivityQueryResponse, ActivityRecordRequest, ActivityRecordResponse, ActivityRoute,
    ActivityScope, ActivitySubject, ActivitySubscribeRequest, ActivitySubscribeResponse,
    ACTIVITY_ACTOR_UNKNOWN, ACTIVITY_CURSOR_INCARNATION_MISMATCH, ACTIVITY_CURSOR_TOO_OLD,
    ACTIVITY_KIND_FORBIDDEN, ACTIVITY_LEDGER_CAPABILITY, ACTIVITY_MALFORMED, ACTIVITY_RATE_LIMITED,
    ACTIVITY_READ_FORBIDDEN, ACTIVITY_SUBSCRIBER_LAGGED, ACTIVITY_TOO_LARGE, ACTIVITY_UNAVAILABLE,
};
use subc_protocol::{ErrorBody, Frame, FrameType};
use tokio::{sync::oneshot, time::Instant};

use crate::{consumer::Subscription, CallError, ModuleHandle, CATALOG_UPDATE_TIMEOUT};

/// A daemon activity refusal. Unknown codes remain readable for forward compatibility.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ActivityRefusal {
    KindForbidden,
    ActorUnknown,
    RateLimited,
    ReadForbidden,
    TooLarge,
    Malformed,
    Unavailable,
    CursorIncarnationMismatch,
    CursorTooOld,
    SubscriberLagged,
    Other(String),
}

impl ActivityRefusal {
    pub fn from_code(code: &str) -> Self {
        match code {
            ACTIVITY_KIND_FORBIDDEN => Self::KindForbidden,
            ACTIVITY_ACTOR_UNKNOWN => Self::ActorUnknown,
            ACTIVITY_RATE_LIMITED => Self::RateLimited,
            ACTIVITY_READ_FORBIDDEN => Self::ReadForbidden,
            ACTIVITY_TOO_LARGE => Self::TooLarge,
            ACTIVITY_MALFORMED => Self::Malformed,
            ACTIVITY_UNAVAILABLE => Self::Unavailable,
            ACTIVITY_CURSOR_INCARNATION_MISMATCH => Self::CursorIncarnationMismatch,
            ACTIVITY_CURSOR_TOO_OLD => Self::CursorTooOld,
            ACTIVITY_SUBSCRIBER_LAGGED => Self::SubscriberLagged,
            other => Self::Other(other.to_owned()),
        }
    }

    pub fn code(&self) -> &str {
        match self {
            Self::KindForbidden => ACTIVITY_KIND_FORBIDDEN,
            Self::ActorUnknown => ACTIVITY_ACTOR_UNKNOWN,
            Self::RateLimited => ACTIVITY_RATE_LIMITED,
            Self::ReadForbidden => ACTIVITY_READ_FORBIDDEN,
            Self::TooLarge => ACTIVITY_TOO_LARGE,
            Self::Malformed => ACTIVITY_MALFORMED,
            Self::Unavailable => ACTIVITY_UNAVAILABLE,
            Self::CursorIncarnationMismatch => ACTIVITY_CURSOR_INCARNATION_MISMATCH,
            Self::CursorTooOld => ACTIVITY_CURSOR_TOO_OLD,
            Self::SubscriberLagged => ACTIVITY_SUBSCRIBER_LAGGED,
            Self::Other(code) => code,
        }
    }
}

/// Recording never retries: a lost reply does not imply that the append failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum ActivityRecordError {
    /// The HELLO_ACK lacked `activity-ledger/v1`; nothing was sent.
    NotSupported,
    Refused {
        reason: ActivityRefusal,
        body: Box<ErrorBody>,
    },
    /// The daemon may have appended the event before this deadline or connection loss.
    Timeout,
    ConnectionClosed,
    Protocol(String),
}

impl ActivityRecordError {
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Refused { body, .. } => Some(&body.code),
            _ => None,
        }
    }
}

impl fmt::Display for ActivityRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSupported => f.write_str("daemon does not support activity-ledger/v1"),
            Self::Refused { body, .. } => write!(
                f,
                "activity.record refused: {}: {}",
                body.code, body.message
            ),
            Self::Timeout => f.write_str("activity.record timed out; append outcome unknown"),
            Self::ConnectionClosed => {
                f.write_str("activity.record connection closed; append outcome unknown")
            }
            Self::Protocol(message) => write!(f, "activity.record protocol error: {message}"),
        }
    }
}
impl Error for ActivityRecordError {}

impl ModuleHandle {
    /// Append one event through the module connection, after checking the negotiated
    /// capability. The daemon verifies citations and stamps the actor. Use ids in
    /// summaries and details, never secret values. A successful reply is durable;
    /// timeout and connection loss must not be retried blindly.
    pub async fn record_activity(
        &self,
        request: ActivityRecordRequest,
    ) -> Result<ActivityRecordResponse, ActivityRecordError> {
        if !self.shared.supports_activity {
            return Err(ActivityRecordError::NotSupported);
        }
        let body = serde_json::to_vec(&request)
            .map_err(|err| ActivityRecordError::Protocol(err.to_string()))?;
        let (corr, writer, rx) = {
            let mut inner = self.shared.lock_inner();
            if inner.closed {
                return Err(ActivityRecordError::ConnectionClosed);
            }
            let writer = inner
                .writer
                .clone()
                .ok_or(ActivityRecordError::ConnectionClosed)?;
            let corr = crate::next_module_control_corr(&mut inner)
                .ok_or(ActivityRecordError::ConnectionClosed)?;
            let (tx, rx) = oneshot::channel();
            inner.pending_activity_records.insert(corr, tx);
            (corr, writer, rx)
        };
        // Cancellation removes the waiter even when the caller drops mid-send.
        let _guard = RecordWaiter {
            handle: self.clone(),
            corr,
        };
        let frame = Frame::build_with_version(
            self.shared.negotiated_ver,
            FrameType::Request,
            crate::control_flags(),
            0,
            0,
            corr,
            body,
        )
        .map_err(|err| ActivityRecordError::Protocol(err.to_string()))?;
        let deadline = Instant::now() + CATALOG_UPDATE_TIMEOUT;
        let reply = tokio::time::timeout_at(deadline, async {
            writer
                .send(frame)
                .await
                .map_err(|_| ActivityRecordError::ConnectionClosed)?;
            rx.await.map_err(|_| ActivityRecordError::ConnectionClosed)
        })
        .await
        .map_err(|_| ActivityRecordError::Timeout)??;
        match reply.header.ty {
            FrameType::Response => serde_json::from_slice(&reply.body)
                .map_err(|err| ActivityRecordError::Protocol(err.to_string())),
            FrameType::Error => {
                let body: ErrorBody = serde_json::from_slice(&reply.body)
                    .map_err(|err| ActivityRecordError::Protocol(err.to_string()))?;
                Err(ActivityRecordError::Refused {
                    reason: ActivityRefusal::from_code(&body.code),
                    body: Box::new(body),
                })
            }
            other => Err(ActivityRecordError::Protocol(format!(
                "unexpected terminal frame: {other:?}"
            ))),
        }
    }
}

struct RecordWaiter {
    handle: ModuleHandle,
    corr: u64,
}
impl Drop for RecordWaiter {
    fn drop(&mut self) {
        self.handle
            .shared
            .lock_inner()
            .pending_activity_records
            .remove(&self.corr);
    }
}

/// A held activity feed. Entries queued before a terminal error are yielded first.
/// Drop or unsubscribe to cancel; reconnect by querying and using the returned head.
pub struct ActivitySubscription {
    pub(crate) inner: Subscription,
    pub(crate) finished: bool,
}

impl ActivitySubscription {
    /// Read the next durable entry. Cursor refusals and lag carry the daemon's code.
    /// After a terminal result, every subsequent call returns `Ok(None)`.
    pub async fn next(&mut self) -> Result<Option<ActivityEntry>, ActivityStreamError> {
        if self.finished {
            return Ok(None);
        }
        if let Some(body) = self.inner.events().recv().await {
            return serde_json::from_slice::<ActivitySubscribeResponse>(&body)
                .map(|response| Some(response.entry))
                .map_err(|err| {
                    self.finished = true;
                    let _ = self.inner.unsubscribe();
                    ActivityStreamError::Decode(err.to_string())
                });
        }
        let terminal = self.inner.closed().await;
        self.finished = true;
        match terminal {
            Ok(()) => Ok(None),
            Err(err) => Err(ActivityStreamError::from_call_error(err)),
        }
    }

    pub fn unsubscribe(&self) -> Result<(), CallError> {
        self.inner.unsubscribe()
    }
}

/// A terminal daemon refusal, transport failure, or invalid entry in an activity feed.
#[derive(Debug)]
#[non_exhaustive]
pub enum ActivityStreamError {
    Refused {
        reason: ActivityRefusal,
        body: Box<ErrorBody>,
    },
    Call(CallError),
    Decode(String),
}

impl ActivityStreamError {
    pub(crate) fn from_call_error(err: CallError) -> Self {
        match err {
            CallError::Module(body) => Self::Refused {
                reason: ActivityRefusal::from_code(&body.code),
                body: Box::new(body),
            },
            other => Self::Call(other),
        }
    }

    /// The daemon's original code, including unknown future refusal codes.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Refused { body, .. } => Some(&body.code),
            Self::Call(err) => err.code(),
            Self::Decode(_) => None,
        }
    }
}

impl fmt::Display for ActivityStreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused { body, .. } => write!(
                f,
                "activity stream refused: {}: {}",
                body.code, body.message
            ),
            Self::Call(err) => err.fmt(f),
            Self::Decode(message) => write!(f, "invalid activity stream entry: {message}"),
        }
    }
}
impl Error for ActivityStreamError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Call(err) => Some(err),
            _ => None,
        }
    }
}

#[cfg(test)]
#[path = "activity_tests.rs"]
mod tests;
