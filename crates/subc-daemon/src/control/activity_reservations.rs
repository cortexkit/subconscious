//! `activity.subscribe` correlations reserved at ingress.
//!
//! The connection reader hands `activity.subscribe` to a task, because
//! opening a ledger subscription can wait on disk, and handles the next frame
//! itself. A `CANCEL` for the same correlation can therefore be handled before
//! the subscription is installed. The reader reserves the correlation before
//! handing the request off: a cancel that finds the reservation marks it
//! cancelled and settles the stream with its STREAM_END, and the subscribe
//! then installs nothing.

use super::*;

/// What has become of a reserved correlation's stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    /// Still being set up: a CANCEL may claim it.
    Pending,
    /// A CANCEL claimed it and sent the stream's one STREAM_END.
    Cancelled,
    /// The subscribe was refused, and its ERROR is the stream's one terminal.
    Refused,
}

type Reserved = Arc<Mutex<HashMap<(ConnectionId, u64), Stream>>>;

/// Reserved correlations and what has become of each one's stream.
#[derive(Clone, Default)]
pub(super) struct ActivityReservations(Reserved);

/// Held by the task serving a reserved `activity.subscribe`. Dropping it
/// releases the reservation, whether the subscription was installed, refused,
/// cancelled, or never started because the connection closed first.
pub(crate) struct ActivityReservation {
    reserved: Reserved,
    key: (ConnectionId, u64),
}

impl Drop for ActivityReservation {
    fn drop(&mut self) {
        self.reserved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.key);
    }
}

impl ActivityReservations {
    /// Claim a pending reservation for a CANCEL. Returns false when there is
    /// none or another CANCEL already claimed it, so the stream settles once.
    /// The caller holds the activity stream lock, which a subscribe also holds
    /// from its own check of [`Self::cancelled`] until it installs its feed.
    pub(super) fn cancel(&self, key: (ConnectionId, u64)) -> bool {
        let mut reserved = self.0.lock().unwrap_or_else(|p| p.into_inner());
        match reserved.get_mut(&key) {
            Some(stream) if *stream == Stream::Pending => {
                *stream = Stream::Cancelled;
                true
            }
            _ => false,
        }
    }

    /// Settle a subscribe's refusal `replies` (empty when the subscription
    /// was installed) against its reservation, so its correlation ends once:
    /// a CANCEL that already claimed the reservation sent the STREAM_END, so
    /// the refusal is dropped; otherwise the refusal ends the stream and a
    /// later CANCEL finds nothing to cancel. A request without a reservation
    /// keeps its replies.
    pub(super) fn settle(&self, key: (ConnectionId, u64), replies: Vec<Frame>) -> Vec<Frame> {
        if replies.is_empty() {
            return replies;
        }
        let mut reserved = self.0.lock().unwrap_or_else(|p| p.into_inner());
        match reserved.get_mut(&key) {
            Some(Stream::Cancelled) => Vec::new(),
            Some(stream) => {
                *stream = Stream::Refused;
                replies
            }
            None => replies,
        }
    }

    /// Whether a CANCEL has claimed this correlation's reservation.
    pub(super) fn cancelled(&self, key: (ConnectionId, u64)) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .is_some_and(|stream| *stream == Stream::Cancelled)
    }
}

impl ControlHandler {
    /// Reserve an `activity.subscribe` request's correlation before the reader
    /// hands it to a task. `None` for any other frame, or when the correlation
    /// is already reserved by an earlier request on this connection.
    pub(crate) fn reserve_activity_subscription(
        &self,
        connection_id: ConnectionId,
        frame: &Frame,
    ) -> Option<impl Send + 'static> {
        let subscribe = frame.header.channel == 0
            && frame.header.ty == FrameType::Request
            && serde_json::from_slice::<ControlOpProbe>(&frame.body)
                .is_ok_and(|probe| probe.op == ops::ACTIVITY_SUBSCRIBE);
        if !subscribe {
            return None;
        }
        let key = (connection_id, frame.header.corr);
        let reserved = &self.activity_reservations.0;
        let mut map = reserved.lock().unwrap_or_else(|p| p.into_inner());
        if map.contains_key(&key) {
            return None;
        }
        map.insert(key, Stream::Pending);
        Some(ActivityReservation {
            reserved: Arc::clone(reserved),
            key,
        })
    }
}
