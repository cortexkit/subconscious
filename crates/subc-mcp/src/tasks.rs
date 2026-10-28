//! MCP tasks for hosts on protocol 2026-07-28 or later.
//!
//! The 2026-07-28 revision moved tasks into the `io.modelcontextprotocol/tasks`
//! extension. A host that declares the extension in a request's `_meta`
//! client capabilities lets the server answer that tools/call with a task
//! handle (`resultType: "task"`) instead of a result. The host then polls
//! `tasks/get` for the task's status and result, answers module questions
//! with `tasks/update`, and may stop the work with `tasks/cancel`.
//!
//! Here a task is only a view of one module call that is already under way:
//! the gateway hands the call it was waiting on to a task when the call runs
//! past `TASK_PROMOTION_DELAY`, or at once when the module asks the host a
//! question, as long as the session has room for another task (see
//! `MAX_TASKS_PER_SESSION`; without room the call stays inline). Nothing is
//! sent to the module again for a task. Cancelling the
//! task cancels the module call exactly as a host cancelling its tools/call
//! does, and a call the gateway loses track of (its route closed, the daemon
//! connection dropped) ends the task as `failed`, never in a retry.
//!
//! Status reaches the host only by polling. The extension also lets a server
//! push `notifications/tasks` on a `subscriptions/listen` stream that names
//! the task, but rmcp 3.8 cannot serve that yet: its subscription filter has
//! no `taskIds`, its acknowledgement cannot list accepted task ids, and its
//! subscription sink refuses task notifications.
//!
//! Task state lives in this process and ends with the host's session: when
//! the session ends, every task's module call is cancelled, as an in-flight
//! tools/call's is when its host goes away.

use super::held_call::{HeldCall, MRTR_INPUT_KEY};
use super::*;
use rmcp::model::{CreateTaskResult, DetailedTask, GetTaskResult, Task, TaskPayload};
use rmcp::task_manager::current_timestamp;

/// How long a tools/call from a tasks-capable host is answered inline before
/// the call becomes a task. Over stdio, holding a request open for a few
/// seconds costs nothing, and most tool calls (a search, a build step) finish
/// within a second or two; answering those inline spares the host a task
/// handle and a round of polling. Only a call that runs longer becomes a
/// task. A module question turns the call into a task at once, whatever the
/// time. Both need a free task slot; without one the call stays inline.
const TASK_PROMOTION_DELAY: Duration = Duration::from_secs(5);
/// Overrides `TASK_PROMOTION_DELAY`, in milliseconds. The tests set it so a
/// call becomes a task without waiting five seconds.
const TASK_PROMOTION_DELAY_MS_ENV: &str = "SUBC_MCP_TASK_PROMOTION_DELAY_MS";
/// The polling interval suggested to the host in every task it is shown
/// (`pollIntervalMs`). Polling is the only way the host learns a task's
/// status (see the module comment), so the interval is short; a `tasks/get`
/// is a lookup in memory and costs the gateway next to nothing.
const TASK_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How long a finished task's status and result are kept for the host to
/// read. The extension leaves the retention to the server: it sets no number,
/// says a server need not keep tasks indefinitely, and has the server report
/// the retention in `ttlMs`. A host polling at `TASK_POLL_INTERVAL` reads a
/// result within about a second of the task finishing, so five minutes (300
/// polling intervals, and the default retention rmcp's own task runtime
/// uses) covers a host that stops polling for a while without keeping
/// results that nobody will read.
const TASK_RESULT_TTL: Duration = Duration::from_secs(5 * 60);
/// Overrides `TASK_RESULT_TTL`, in milliseconds. The tests set it to check
/// that an expired task is reported as expired.
const TASK_RESULT_TTL_MS_ENV: &str = "SUBC_MCP_TASK_RESULT_TTL_MS";
/// Most tasks one MCP session keeps at once, running and finished. A
/// finished task holds its whole tool result until it expires, so the cap
/// bounds that memory. 32 is far more calls than an agent runs in parallel,
/// and matches the gateway's cap on conversation routes per connection.
/// When the cap is reached, the oldest finished task is dropped early (and
/// reported as expired); when every task is still running, a further call
/// stays inline, as it would for a host without tasks.
const MAX_TASKS_PER_SESSION: usize = 32;
/// Every task id the gateway mints is this prefix and a sequence number, so a
/// task id the session minted earlier but no longer keeps can be told apart
/// from one it never minted, and reported as expired.
const TASK_ID_PREFIX: &str = "subc-mcp-task-";
/// Answers queued for one task while the gateway passes earlier ones to the
/// module. A module asks one question at a time, so a few slots are plenty;
/// when they are full, `tasks/update` asks the host to send again shortly
/// instead of queueing without bound.
const TASK_UPDATE_QUEUE: usize = 4;

/// A call `Tasks::start` had no room for, handed back with its question (if
/// any) so the caller can keep answering it inline.
pub(super) type Declined = (Box<HeldCall>, Option<InputAsk>);

#[derive(Debug, Clone, Copy)]
struct TaskSettings {
    promotion_delay: Duration,
    result_ttl: Duration,
}

impl TaskSettings {
    fn from_env() -> Self {
        let millis = |key: &str| {
            env::var(key)
                .ok()
                .and_then(|raw| raw.trim().parse::<u64>().ok())
                .map(Duration::from_millis)
        };
        Self {
            promotion_delay: millis(TASK_PROMOTION_DELAY_MS_ENV).unwrap_or(TASK_PROMOTION_DELAY),
            result_ttl: millis(TASK_RESULT_TTL_MS_ENV)
                .filter(|ttl| !ttl.is_zero())
                .unwrap_or(TASK_RESULT_TTL),
        }
    }
}

#[derive(Debug, Clone)]
enum TaskState {
    Working,
    InputRequired(InputRequests),
    Completed(JsonObject),
    Failed(JsonObject),
    Cancelled,
}

impl TaskState {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed(_) | Self::Failed(_) | Self::Cancelled)
    }

    fn payload(&self) -> TaskPayload {
        match self {
            Self::Working => TaskPayload::Working,
            Self::InputRequired(input_requests) => TaskPayload::InputRequired {
                input_requests: input_requests.clone(),
            },
            Self::Completed(result) => TaskPayload::Completed {
                result: result.clone(),
            },
            Self::Failed(error) => TaskPayload::Failed {
                error: error.clone(),
            },
            Self::Cancelled => TaskPayload::Cancelled,
        }
    }
}

struct TaskEntry {
    seq: u64,
    created: time::Instant,
    created_at: String,
    last_updated_at: String,
    state: TaskState,
    /// When the task reached a terminal state; its retention counts from here.
    finished: Option<time::Instant>,
    /// Set by `tasks/cancel`. Dropping it (the session ended) also stops the
    /// task's driver, which then cancels the module call.
    cancel: watch::Sender<bool>,
    answers: mpsc::Sender<InputResponses>,
}

impl TaskEntry {
    fn task(&self, task_id: &str, result_ttl: Duration) -> Task {
        let task = Task::new(
            task_id,
            self.state.payload().status(),
            self.created_at.clone(),
            self.last_updated_at.clone(),
        )
        .with_poll_interval_ms(duration_ms(TASK_POLL_INTERVAL));
        // `ttlMs` counts from creation. A running task is kept for as long
        // as its module call runs, which no number can promise, so it is
        // reported as unlimited (`null`) until the task finishes; from then
        // on it is the moment the task will be dropped.
        match self.finished {
            Some(finished) => task.with_ttl_ms(duration_ms((finished - self.created) + result_ttl)),
            None => task,
        }
    }

    fn set_state(&mut self, state: TaskState) {
        if self.state.is_terminal() {
            return;
        }
        if state.is_terminal() {
            self.finished = Some(time::Instant::now());
        }
        self.state = state;
        self.last_updated_at = current_timestamp();
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[derive(Default)]
struct TasksInner {
    /// Sequence number of the last task minted; ids up to it were handed out.
    last_seq: u64,
    entries: HashMap<String, TaskEntry>,
}

/// One MCP session's tasks, keyed by task id. A host can reach only its own
/// session's tasks.
#[derive(Clone)]
pub(super) struct Tasks {
    inner: Arc<std::sync::Mutex<TasksInner>>,
    settings: TaskSettings,
}

impl Default for Tasks {
    fn default() -> Self {
        Self {
            inner: Arc::default(),
            settings: TaskSettings::from_env(),
        }
    }
}

impl Tasks {
    fn lock(&self) -> std::sync::MutexGuard<'_, TasksInner> {
        self.inner.lock().unwrap_or_else(|poisoned| {
            tracing::warn!("recovering from poisoned MCP task lock");
            poisoned.into_inner()
        })
    }

    /// When a call sent now becomes a task.
    pub(super) fn promotion_deadline(&self) -> time::Instant {
        time::Instant::now() + self.settings.promotion_delay
    }

    /// Hands `held` to a new task and returns the handle for the host's
    /// tools/call. With `ask`, the module's question is put to the host
    /// through the task's `input_required` status. When the session already
    /// keeps `MAX_TASKS_PER_SESSION` running tasks, the call and its question
    /// come back untouched, for the caller to keep answering inline. They come
    /// back boxed because a question is large, and this path is rare.
    pub(super) fn start(
        &self,
        mut held: Box<HeldCall>,
        ask: Option<InputAsk>,
        subc: SubcClient,
    ) -> std::result::Result<CreateTaskResult, Box<Declined>> {
        let (cancel, cancelled) = watch::channel(false);
        let (answers, answered) = mpsc::channel(TASK_UPDATE_QUEUE);
        let (task_id, created, input_deadline) = {
            let mut inner = self.lock();
            self.sweep(&mut inner);
            if inner.entries.len() >= MAX_TASKS_PER_SESSION && !evict_oldest_finished(&mut inner) {
                tracing::info!(
                    "MCP session already runs {MAX_TASKS_PER_SESSION} tasks; answering the call inline"
                );
                return Err(Box::new((held, ask)));
            }
            let (state, input_deadline) = match ask {
                Some(ask) => {
                    let deadline = held.input.mark_outstanding(&ask);
                    let mut requests = InputRequests::new();
                    requests.insert(MRTR_INPUT_KEY.to_owned(), ask.request);
                    (TaskState::InputRequired(requests), Some(deadline))
                }
                None => (TaskState::Working, None),
            };
            inner.last_seq += 1;
            let seq = inner.last_seq;
            let task_id = format!("{TASK_ID_PREFIX}{seq}");
            let now = current_timestamp();
            let entry = TaskEntry {
                seq,
                created: time::Instant::now(),
                created_at: now.clone(),
                last_updated_at: now,
                state,
                finished: None,
                cancel,
                answers,
            };
            let created = entry.task(&task_id, self.settings.result_ttl);
            inner.entries.insert(task_id.clone(), entry);
            (task_id, created, input_deadline)
        };
        tracing::info!(
            "tool call to '{}' continues as MCP task {task_id}",
            held.module_id
        );
        tokio::spawn(drive_task(
            held,
            input_deadline,
            TaskDriver {
                tasks: self.clone(),
                task_id,
                subc,
            },
            cancelled,
            answered,
        ));
        Ok(CreateTaskResult::new(created))
    }

    /// `tasks/get`: the task's status, with its question, result or error.
    pub(super) fn get(&self, task_id: &str) -> std::result::Result<GetTaskResult, ErrorData> {
        let mut inner = self.lock();
        self.sweep(&mut inner);
        let entry = inner
            .entries
            .get(task_id)
            .ok_or_else(|| self.missing(&inner, task_id))?;
        Ok(GetTaskResult::new(DetailedTask::new(
            entry.task(task_id, self.settings.result_ttl),
            entry.state.payload(),
        )))
    }

    /// `tasks/update`: passes the host's answer to the module's outstanding
    /// question. Answers under other keys, or for a task with no question
    /// outstanding, are ignored, as the extension asks.
    pub(super) fn update(
        &self,
        task_id: &str,
        responses: InputResponses,
    ) -> std::result::Result<(), ErrorData> {
        let mut inner = self.lock();
        self.sweep(&mut inner);
        let entry = inner
            .entries
            .get(task_id)
            .ok_or_else(|| self.missing(&inner, task_id))?;
        if !matches!(entry.state, TaskState::InputRequired(_))
            || !responses.contains_key(MRTR_INPUT_KEY)
        {
            return Ok(());
        }
        entry.answers.try_send(responses).map_err(|_| {
            ErrorData::internal_error(
                "the task is still passing on an earlier answer; send this one again shortly",
                None,
            )
        })
    }

    /// `tasks/cancel`: cancels the module call. The task shows `cancelled`
    /// once its driver has sent the module the cancel (a send that fails is
    /// logged, and the gateway stops listening for the call either way); a
    /// task that already finished keeps its status.
    pub(super) fn cancel(&self, task_id: &str) -> std::result::Result<(), ErrorData> {
        let mut inner = self.lock();
        self.sweep(&mut inner);
        let entry = inner
            .entries
            .get(task_id)
            .ok_or_else(|| self.missing(&inner, task_id))?;
        if !entry.state.is_terminal() {
            let _ = entry.cancel.send(true);
        }
        Ok(())
    }

    /// Drops every task when the host's session ends; each running task's
    /// module call is cancelled by its driver.
    pub(super) fn release_all(&self) {
        let entries = std::mem::take(&mut self.lock().entries);
        drop(entries);
    }

    fn set_state(&self, task_id: &str, state: TaskState) {
        if let Some(entry) = self.lock().entries.get_mut(task_id) {
            entry.set_state(state);
        }
    }

    /// Drops finished tasks whose retention has run out.
    fn sweep(&self, inner: &mut TasksInner) {
        let ttl = self.settings.result_ttl;
        inner.entries.retain(|_, entry| {
            entry
                .finished
                .is_none_or(|finished| finished.elapsed() < ttl)
        });
    }

    /// The error for a task id the session does not keep: expired when the
    /// session minted it, unknown otherwise. Both are invalid params, as the
    /// extension requires.
    fn missing(&self, inner: &TasksInner, task_id: &str) -> ErrorData {
        // Only the exact id the session handed out counts, so a variant such
        // as a leading zero is unknown, not expired.
        let minted = task_id
            .strip_prefix(TASK_ID_PREFIX)
            .and_then(|seq| seq.parse::<u64>().ok())
            .is_some_and(|seq| {
                (1..=inner.last_seq).contains(&seq) && task_id == format!("{TASK_ID_PREFIX}{seq}")
            });
        if minted {
            ErrorData::invalid_params(
                format!(
                    "task {task_id} has expired: the gateway keeps a finished task for {:?}, and at most {MAX_TASKS_PER_SESSION} tasks per MCP session",
                    self.settings.result_ttl
                ),
                Some(serde_json::json!({ "taskId": task_id, "expired": true })),
            )
        } else {
            ErrorData::invalid_params(
                format!("unknown task {task_id}"),
                Some(serde_json::json!({ "taskId": task_id })),
            )
        }
    }
}

/// Makes room for one task by dropping the finished task that finished
/// first. False when every task is still running.
fn evict_oldest_finished(inner: &mut TasksInner) -> bool {
    let oldest = inner
        .entries
        .iter()
        .filter_map(|(task_id, entry)| entry.finished.map(|at| (at, entry.seq, task_id.clone())))
        .min();
    match oldest {
        Some((_, _, task_id)) => inner.entries.remove(&task_id).is_some(),
        None => false,
    }
}

struct TaskDriver {
    tasks: Tasks,
    task_id: String,
    subc: SubcClient,
}

/// Follows one task's module call until it ends: records the module's
/// questions and result in the task, passes the host's answers on, and
/// cancels the call when the host cancels the task or its session ends.
async fn drive_task(
    mut held: Box<HeldCall>,
    mut input_deadline: Option<time::Instant>,
    driver: TaskDriver,
    mut cancelled: watch::Receiver<bool>,
    mut answered: mpsc::Receiver<InputResponses>,
) {
    let TaskDriver {
        tasks,
        task_id,
        subc,
    } = driver;
    loop {
        let unanswered = async {
            match input_deadline {
                Some(deadline) => time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            changed = cancelled.changed() => {
                if let Err(error) = send_module_cancel(&subc, held.route, held.corr).await {
                    tracing::warn!("could not send the cancel for MCP task {task_id}: {error}");
                }
                subc.abandon_request(held.route, held.corr).await;
                // An error means the session ended and nobody reads the task.
                if changed.is_ok() {
                    tasks.set_state(&task_id, TaskState::Cancelled);
                }
                return;
            }
            Some(responses) = answered.recv() => {
                if input_deadline.is_some() {
                    if let Some(answer) = responses.get(MRTR_INPUT_KEY) {
                        held.input.deliver_answer(Some(answer)).await;
                        input_deadline = None;
                        tasks.set_state(&task_id, TaskState::Working);
                    }
                }
            }
            frame = held.frames.next() => {
                let state = match frame {
                    // There is no host request left to report progress on.
                    Some(frame) if frame.header.ty == FrameType::Push => continue,
                    Some(frame) => {
                        task_state_for(frame.tool_call_answer(held.retrying, held.daemon_origin_trusted), &held.module_id)
                    }
                    None => TaskState::Failed(error_object(&route_closed_error(
                        held.route, held.corr,
                    ))),
                };
                tasks.set_state(&task_id, state);
                return;
            }
            ask = held.input.next() => {
                input_deadline = Some(held.input.mark_outstanding(&ask));
                let mut requests = InputRequests::new();
                requests.insert(MRTR_INPUT_KEY.to_owned(), ask.request);
                tasks.set_state(&task_id, TaskState::InputRequired(requests));
            }
            _ = unanswered => {
                held.input.decline_outstanding().await;
                input_deadline = None;
                tasks.set_state(&task_id, TaskState::Working);
            }
        }
    }
}

/// The task status the module's terminal frame gives. A tool result, even
/// one with `isError: true`, completes the task; an outage refusal completes
/// it with the same outage block an inline call returns, because the task
/// never sends the call again; anything the gateway would have answered the
/// tools/call with as a JSON-RPC error fails it with that error.
fn task_state_for(
    answer: std::result::Result<CallAttempt, ErrorData>,
    module_id: &str,
) -> TaskState {
    match answer {
        Ok(CallAttempt::Answered(result)) => TaskState::Completed(result_object(&result)),
        Ok(CallAttempt::NothingSent(refusal)) => {
            TaskState::Completed(result_object(&outage_tool_result(module_id, &refusal)))
        }
        Ok(CallAttempt::RouteGone) => TaskState::Failed(error_object(&ErrorData::internal_error(
            "subc route tool call failed: the route closed before the call was sent",
            None,
        ))),
        Ok(CallAttempt::NeedsInput(..) | CallAttempt::Promote(..)) => {
            TaskState::Failed(error_object(&ErrorData::internal_error(
                "the module's answer could not be read",
                None,
            )))
        }
        Err(error) => TaskState::Failed(error_object(&error)),
    }
}

fn result_object(result: &CallToolResult) -> JsonObject {
    match serde_json::to_value(result) {
        Ok(serde_json::Value::Object(object)) => object,
        _ => JsonObject::new(),
    }
}

fn error_object(error: &ErrorData) -> JsonObject {
    match serde_json::to_value(error) {
        Ok(serde_json::Value::Object(object)) => object,
        _ => JsonObject::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_expired(error: &ErrorData) -> bool {
        error
            .data
            .as_ref()
            .and_then(|data| data.get("expired"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    }

    #[test]
    fn only_an_exact_handed_out_id_is_reported_expired() {
        let tasks = Tasks::default();
        tasks.lock().last_seq = 3;
        let inner = tasks.lock();
        assert!(is_expired(&tasks.missing(&inner, "subc-mcp-task-2")));
        for never_handed_out in [
            "subc-mcp-task-02",
            "subc-mcp-task-+2",
            "subc-mcp-task-4",
            "subc-mcp-task-0",
            "other-2",
        ] {
            assert!(
                !is_expired(&tasks.missing(&inner, never_handed_out)),
                "{never_handed_out} was reported expired"
            );
        }
    }
}
