use std::{error::Error, fmt, future::Future, pin::Pin, sync::Arc};

use rmcp::model::{
    ErrorCode, ErrorData, GetPromptRequestParams, GetPromptResult, JsonObject, ListPromptsResult,
    Prompt, PromptArgument, PromptMessage, PromptMessageRole,
};
use serde_json::Value;

const STATUS_NAME: &str = "status";
const STATUS_DESCRIPTION: &str = "Summarize the current conversation state from Magic Context.";
const WRAPUP_NAME: &str = "wrapup";
const WRAPUP_DESCRIPTION: &str =
    "Wrap up this conversation: fold history and keep only the most recent messages.";
const KEEP_ARGUMENT: &str = "keep";
const KEEP_DESCRIPTION: &str = "number of recent messages to keep (5-100, default 20)";
const BACKEND_UNAVAILABLE: ErrorCode = ErrorCode(-32000);
const MAX_STATUS_SUMMARY_CHARS: usize = 500;

type StatusBackendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<String, PromptBackendError>> + Send + 'a>>;
type WrapupBackendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<WrapupEnqueued, PromptBackendError>> + Send + 'a>>;

/// Prompt backends receive only launch identity and validated prompt parameters.
/// This narrow boundary prevents dispatch from acquiring unrelated application state.
pub(crate) trait PromptBackend: Send + Sync {
    fn status<'a>(&'a self, instance_token: Option<&'a str>) -> StatusBackendFuture<'a>;

    fn enqueue_wrapup<'a>(
        &'a self,
        instance_token: Option<&'a str>,
        keep: Option<i64>,
    ) -> WrapupBackendFuture<'a>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code, reason = "constructed by future backend implementations")]
pub(crate) enum WrapupEnqueueStatus {
    Queued,
    AlreadyQueued,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WrapupEnqueued {
    pub(crate) status: WrapupEnqueueStatus,
    pub(crate) command_id: String,
    pub(crate) keep: u32,
    pub(crate) clamped: bool,
    pub(crate) expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code, reason = "reserved for future backend implementations")]
pub(crate) enum PromptBackendError {
    Unavailable(String),
    InvalidParams(String),
    Internal(String),
}

impl fmt::Display for PromptBackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) | Self::InvalidParams(message) | Self::Internal(message) => {
                formatter.write_str(message)
            }
        }
    }
}

impl Error for PromptBackendError {}

#[derive(Debug, Default)]
pub(crate) struct PendingBackend;

impl PromptBackend for PendingBackend {
    fn status<'a>(&'a self, _instance_token: Option<&'a str>) -> StatusBackendFuture<'a> {
        Box::pin(async {
            Err(PromptBackendError::Unavailable(
                "status backend not wired yet".to_owned(),
            ))
        })
    }

    fn enqueue_wrapup<'a>(
        &'a self,
        _instance_token: Option<&'a str>,
        _keep: Option<i64>,
    ) -> WrapupBackendFuture<'a> {
        Box::pin(async {
            Err(PromptBackendError::Unavailable(
                "wrapup backend not wired yet".to_owned(),
            ))
        })
    }
}

#[derive(Clone)]
pub(crate) struct PromptService {
    instance_token: Option<String>,
    backend: Arc<dyn PromptBackend>,
}

pub(crate) fn prompt_names() -> [&'static str; 2] {
    [STATUS_NAME, WRAPUP_NAME]
}

impl PromptService {
    pub(crate) fn new(instance_token: Option<String>, backend: Arc<dyn PromptBackend>) -> Self {
        Self {
            instance_token,
            backend,
        }
    }

    pub(crate) fn list_prompts(
        &self,
        mut is_visible: impl FnMut(&str) -> bool,
    ) -> ListPromptsResult {
        ListPromptsResult {
            prompts: prompt_descriptors()
                .into_iter()
                .filter(|prompt| is_visible(&prompt.name))
                .collect(),
            ..Default::default()
        }
    }

    pub(crate) async fn get_prompt_if_visible(
        &self,
        request: GetPromptRequestParams,
        is_visible: bool,
    ) -> Result<GetPromptResult, ErrorData> {
        if !is_visible {
            return Err(unknown_prompt(&request.name));
        }
        self.get_prompt(request).await
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
    ) -> Result<GetPromptResult, ErrorData> {
        let arguments = request.arguments.as_ref();
        let text = match request.name.as_str() {
            STATUS_NAME => {
                reject_arguments(STATUS_NAME, arguments)?;
                let summary = self
                    .backend
                    .status(self.instance_token.as_deref())
                    .await
                    .map_err(backend_error_to_mcp)?;
                validate_status_summary(&summary)?;
                summary
            }
            WRAPUP_NAME => {
                let keep = parse_wrapup_keep(arguments)?;
                let enqueued = self
                    .backend
                    .enqueue_wrapup(self.instance_token.as_deref(), keep)
                    .await
                    .map_err(backend_error_to_mcp)?;
                log_wrapup(&enqueued);
                render_wrapup(enqueued)
            }
            name => return Err(unknown_prompt(name)),
        };

        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            text,
        )]))
    }
}

fn prompt_descriptors() -> Vec<Prompt> {
    vec![
        Prompt::new(
            STATUS_NAME,
            Some(STATUS_DESCRIPTION),
            None::<Vec<PromptArgument>>,
        ),
        Prompt::new(
            WRAPUP_NAME,
            Some(WRAPUP_DESCRIPTION),
            Some(vec![PromptArgument::new(KEEP_ARGUMENT)
                .with_description(KEEP_DESCRIPTION)
                .with_required(false)]),
        ),
    ]
}

fn reject_arguments(prompt_name: &str, arguments: Option<&JsonObject>) -> Result<(), ErrorData> {
    let Some(name) = arguments.and_then(|arguments| arguments.keys().next()) else {
        return Ok(());
    };
    Err(invalid_params(format!(
        "unknown argument '{name}' for prompt '{prompt_name}'"
    )))
}

fn parse_wrapup_keep(arguments: Option<&JsonObject>) -> Result<Option<i64>, ErrorData> {
    parse_wrapup_keep_pairs(
        arguments
            .into_iter()
            .flat_map(|arguments| arguments.iter().map(|(name, value)| (name.as_str(), value))),
    )
}

// MCP arguments are decoded into JsonObject before this handler runs, so duplicate
// wire keys have already collapsed. The pair check remains defensive for callers
// that preserve duplicates, while unknown decoded arguments still fail closed.
fn parse_wrapup_keep_pairs<'a>(
    arguments: impl IntoIterator<Item = (&'a str, &'a Value)>,
) -> Result<Option<i64>, ErrorData> {
    let mut keep = None;
    for (name, value) in arguments {
        if name != KEEP_ARGUMENT {
            return Err(invalid_params(format!(
                "unknown argument '{name}' for prompt '{WRAPUP_NAME}'"
            )));
        }
        if keep.is_some() {
            return Err(invalid_params(format!(
                "duplicate argument '{KEEP_ARGUMENT}' for prompt '{WRAPUP_NAME}'"
            )));
        }

        let raw = value
            .as_str()
            .ok_or_else(|| invalid_params("keep must be an integer"))?;
        keep = Some(
            raw.parse::<i64>()
                .map_err(|_| invalid_params("keep must be an integer"))?,
        );
    }

    Ok(keep)
}

fn validate_status_summary(summary: &str) -> Result<(), ErrorData> {
    if summary.chars().count() > MAX_STATUS_SUMMARY_CHARS
        || summary.contains('\n')
        || summary.contains('\r')
    {
        return Err(ErrorData::internal_error(
            "prompt backend returned an invalid status summary",
            None,
        ));
    }
    Ok(())
}

fn log_wrapup(enqueued: &WrapupEnqueued) {
    eprintln!(
        "subc-mcp prompt wrapup: status={:?} command_id={} effective_keep={} clamped={} expires_at_ms={}",
        enqueued.status,
        enqueued.command_id,
        enqueued.keep,
        enqueued.clamped,
        enqueued.expires_at_ms
    );
}

fn render_wrapup(enqueued: WrapupEnqueued) -> String {
    let status = match enqueued.status {
        WrapupEnqueueStatus::Queued => "Wrapup queued",
        WrapupEnqueueStatus::AlreadyQueued => "Wrapup already queued",
    };
    let clamped = if enqueued.clamped {
        " (clamped from your requested value)"
    } else {
        ""
    };
    format!(
        "{status} with an effective keep of {} messages{clamped}. It starts on the next eligible parent message, the fold takes effect on a later pass, and an abandoned command expires after its 15-minute deadline.",
        enqueued.keep
    )
}

fn backend_error_to_mcp(error: PromptBackendError) -> ErrorData {
    match error {
        PromptBackendError::Unavailable(_) => {
            ErrorData::new(BACKEND_UNAVAILABLE, "prompt backend is unavailable", None)
        }
        PromptBackendError::InvalidParams(_) => {
            ErrorData::invalid_params("prompt backend rejected the request", None)
        }
        PromptBackendError::Internal(_) => ErrorData::internal_error("prompt backend failed", None),
    }
}

fn unknown_prompt(name: &str) -> ErrorData {
    invalid_params(format!("unknown prompt '{name}'"))
}

fn invalid_params(message: impl Into<std::borrow::Cow<'static, str>>) -> ErrorData {
    ErrorData::invalid_params(message, None)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum BackendCall {
        Status {
            instance_token: Option<String>,
        },
        Wrapup {
            instance_token: Option<String>,
            keep: Option<i64>,
        },
    }

    struct MockBackend {
        status_response: Result<String, PromptBackendError>,
        wrapup_response: Result<WrapupEnqueued, PromptBackendError>,
        calls: Mutex<Vec<BackendCall>>,
    }

    impl MockBackend {
        fn success(text: &str) -> Self {
            Self {
                status_response: Ok(text.to_owned()),
                wrapup_response: Ok(queued_wrapup()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn with_wrapup(wrapup: WrapupEnqueued) -> Self {
            Self {
                status_response: Ok("unused".to_owned()),
                wrapup_response: Ok(wrapup),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn failure(error: PromptBackendError) -> Self {
            Self {
                status_response: Err(error.clone()),
                wrapup_response: Err(error),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<BackendCall> {
            self.calls.lock().expect("call lock poisoned").clone()
        }
    }

    impl PromptBackend for MockBackend {
        fn status<'a>(&'a self, instance_token: Option<&'a str>) -> StatusBackendFuture<'a> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .expect("call lock poisoned")
                    .push(BackendCall::Status {
                        instance_token: instance_token.map(str::to_owned),
                    });
                self.status_response.clone()
            })
        }

        fn enqueue_wrapup<'a>(
            &'a self,
            instance_token: Option<&'a str>,
            keep: Option<i64>,
        ) -> WrapupBackendFuture<'a> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .expect("call lock poisoned")
                    .push(BackendCall::Wrapup {
                        instance_token: instance_token.map(str::to_owned),
                        keep,
                    });
                self.wrapup_response.clone()
            })
        }
    }

    fn queued_wrapup() -> WrapupEnqueued {
        WrapupEnqueued {
            status: WrapupEnqueueStatus::Queued,
            command_id: "command-123".to_owned(),
            keep: 20,
            clamped: false,
            expires_at_ms: 900_000,
        }
    }

    fn service(backend: Arc<MockBackend>) -> PromptService {
        PromptService::new(Some("instance-token-123".to_owned()), backend)
    }

    fn wrapup_request(keep: Option<&str>) -> GetPromptRequestParams {
        let request = GetPromptRequestParams::new(WRAPUP_NAME);
        match keep {
            Some(keep) => {
                let mut arguments = JsonObject::new();
                arguments.insert(KEEP_ARGUMENT.to_owned(), json!(keep));
                request.with_arguments(arguments)
            }
            None => request,
        }
    }

    #[test]
    fn prompt_descriptors_serialization_is_exact() {
        let backend = Arc::new(MockBackend::success("unused"));
        let serialized = serde_json::to_value(service(backend).list_prompts(|_| true)).unwrap();
        assert_eq!(
            serialized,
            json!({
                "prompts": [
                    {
                        "name": "status",
                        "description": "Summarize the current conversation state from Magic Context."
                    },
                    {
                        "name": "wrapup",
                        "description": "Wrap up this conversation: fold history and keep only the most recent messages.",
                        "arguments": [
                            {
                                "name": "keep",
                                "description": "number of recent messages to keep (5-100, default 20)",
                                "required": false
                            }
                        ]
                    }
                ]
            })
        );
    }

    #[test]
    fn prompt_list_filter_can_hide_every_descriptor() {
        let backend = Arc::new(MockBackend::success("unused"));
        let result = service(backend).list_prompts(|_| false);
        assert!(result.prompts.is_empty());
    }

    #[tokio::test]
    async fn hidden_prompt_get_is_inert() {
        let backend = Arc::new(MockBackend::success("must not be returned"));
        let error = service(Arc::clone(&backend))
            .get_prompt_if_visible(GetPromptRequestParams::new(STATUS_NAME), false)
            .await
            .unwrap_err();

        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        assert_eq!(error.message, "unknown prompt 'status'");
        assert!(backend.calls().is_empty());
    }

    #[tokio::test]
    async fn prompt_success_returns_one_user_text_message_and_passes_instance_token() {
        let backend = Arc::new(MockBackend::success("SUMMARY"));
        let result = service(Arc::clone(&backend))
            .get_prompt(GetPromptRequestParams::new(STATUS_NAME))
            .await
            .unwrap();

        assert_eq!(
            serde_json::to_value(result).unwrap(),
            json!({
                "messages": [
                    {
                        "role": "user",
                        "content": { "type": "text", "text": "SUMMARY" }
                    }
                ]
            })
        );
        assert_eq!(
            backend.calls(),
            vec![BackendCall::Status {
                instance_token: Some("instance-token-123".to_owned())
            }]
        );
    }

    #[tokio::test]
    async fn wrapup_keep_passes_absence_and_signed_integers_to_backend() {
        let backend = Arc::new(MockBackend::success("unused"));
        let service = service(Arc::clone(&backend));

        service.get_prompt(wrapup_request(None)).await.unwrap();
        service
            .get_prompt(wrapup_request(Some("-2")))
            .await
            .unwrap();
        service.get_prompt(wrapup_request(Some("4"))).await.unwrap();
        service
            .get_prompt(wrapup_request(Some("500")))
            .await
            .unwrap();

        assert_eq!(
            backend.calls(),
            vec![
                BackendCall::Wrapup {
                    instance_token: Some("instance-token-123".to_owned()),
                    keep: None,
                },
                BackendCall::Wrapup {
                    instance_token: Some("instance-token-123".to_owned()),
                    keep: Some(-2),
                },
                BackendCall::Wrapup {
                    instance_token: Some("instance-token-123".to_owned()),
                    keep: Some(4),
                },
                BackendCall::Wrapup {
                    instance_token: Some("instance-token-123".to_owned()),
                    keep: Some(500),
                },
            ]
        );
    }

    #[tokio::test]
    async fn wrapup_keep_rejects_malformed_and_overflow_values() {
        let backend = Arc::new(MockBackend::success("unused"));
        let service = service(backend);

        for raw in ["abc", "9223372036854775808"] {
            let error = service
                .get_prompt(wrapup_request(Some(raw)))
                .await
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
            assert_eq!(error.message, "keep must be an integer");
        }
    }

    #[tokio::test]
    async fn wrapup_result_renders_clamped_and_already_queued_states_distinctly() {
        let clamped = WrapupEnqueued {
            status: WrapupEnqueueStatus::Queued,
            command_id: "command-clamped".to_owned(),
            keep: 5,
            clamped: true,
            expires_at_ms: 123_456,
        };
        let queued = service(Arc::new(MockBackend::with_wrapup(clamped)))
            .get_prompt(wrapup_request(Some("-2")))
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(queued)
                .unwrap()
                .pointer("/messages/0/content/text")
                .and_then(Value::as_str),
            Some(
                "Wrapup queued with an effective keep of 5 messages (clamped from your requested value). It starts on the next eligible parent message, the fold takes effect on a later pass, and an abandoned command expires after its 15-minute deadline."
            )
        );

        let already_queued = WrapupEnqueued {
            status: WrapupEnqueueStatus::AlreadyQueued,
            command_id: "command-existing".to_owned(),
            keep: 20,
            clamped: false,
            expires_at_ms: 654_321,
        };
        let already_queued = service(Arc::new(MockBackend::with_wrapup(already_queued)))
            .get_prompt(wrapup_request(None))
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(already_queued)
                .unwrap()
                .pointer("/messages/0/content/text")
                .and_then(Value::as_str),
            Some(
                "Wrapup already queued with an effective keep of 20 messages. It starts on the next eligible parent message, the fold takes effect on a later pass, and an abandoned command expires after its 15-minute deadline."
            )
        );
    }

    #[test]
    fn defensive_pair_parser_rejects_duplicate_and_unknown_argument_names() {
        let first = json!("20");
        let second = json!("30");
        let duplicate =
            parse_wrapup_keep_pairs([(KEEP_ARGUMENT, &first), (KEEP_ARGUMENT, &second)])
                .unwrap_err();
        assert_eq!(duplicate.code, ErrorCode::INVALID_PARAMS);
        assert_eq!(
            duplicate.message,
            "duplicate argument 'keep' for prompt 'wrapup'"
        );

        let unknown = json!("20");
        let unknown = parse_wrapup_keep_pairs([("recent", &unknown)]).unwrap_err();
        assert_eq!(unknown.code, ErrorCode::INVALID_PARAMS);
        assert_eq!(
            unknown.message,
            "unknown argument 'recent' for prompt 'wrapup'"
        );
    }

    #[tokio::test]
    async fn unknown_prompt_and_status_arguments_return_invalid_params() {
        let backend = Arc::new(MockBackend::success("unused"));
        let service = service(backend);

        let error = service
            .get_prompt(GetPromptRequestParams::new("missing"))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        assert_eq!(error.message, "unknown prompt 'missing'");

        let mut arguments = JsonObject::new();
        arguments.insert("keep".to_owned(), json!("20"));
        let error = service
            .get_prompt(GetPromptRequestParams::new(STATUS_NAME).with_arguments(arguments))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        assert_eq!(error.message, "unknown argument 'keep' for prompt 'status'");
    }

    #[tokio::test]
    async fn status_summary_rejects_oversize_and_multiline_backend_text() {
        for summary in ["x".repeat(501), "first line\nsecond line".to_owned()] {
            let backend = Arc::new(MockBackend::success(&summary));
            let error = service(backend)
                .get_prompt(GetPromptRequestParams::new(STATUS_NAME))
                .await
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
            assert_eq!(
                error.message,
                "prompt backend returned an invalid status summary"
            );
        }
    }

    #[tokio::test]
    async fn status_summary_accepts_exactly_five_hundred_characters() {
        let summary = "x".repeat(MAX_STATUS_SUMMARY_CHARS);
        let backend = Arc::new(MockBackend::success(&summary));
        let result = service(backend)
            .get_prompt(GetPromptRequestParams::new(STATUS_NAME))
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(result)
                .unwrap()
                .pointer("/messages/0/content/text")
                .and_then(Value::as_str),
            Some(summary.as_str())
        );
    }

    #[tokio::test]
    async fn backend_failure_returns_clean_mcp_error() {
        let backend = Arc::new(MockBackend::failure(PromptBackendError::Unavailable(
            "temporarily offline".to_owned(),
        )));
        let error = service(backend)
            .get_prompt(GetPromptRequestParams::new(STATUS_NAME))
            .await
            .unwrap_err();

        assert_eq!(error.code, BACKEND_UNAVAILABLE);
        assert_eq!(error.message, "prompt backend is unavailable");
    }

    #[test]
    fn backend_error_variants_map_to_distinct_mcp_errors() {
        let invalid = backend_error_to_mcp(PromptBackendError::InvalidParams(
            "backend rejected parameters".to_owned(),
        ));
        assert_eq!(invalid.code, ErrorCode::INVALID_PARAMS);
        assert_eq!(invalid.message, "prompt backend rejected the request");

        let internal = backend_error_to_mcp(PromptBackendError::Internal(
            "backend response failed".to_owned(),
        ));
        assert_eq!(internal.code, ErrorCode::INTERNAL_ERROR);
        assert_eq!(internal.message, "prompt backend failed");
    }

    #[tokio::test]
    async fn repeated_prompt_get_calls_do_not_cache_backend_results() {
        let backend = Arc::new(MockBackend::success("SUMMARY"));
        let service = service(Arc::clone(&backend));
        let request = wrapup_request(Some("20"));

        service.get_prompt(request.clone()).await.unwrap();
        service.get_prompt(request).await.unwrap();

        assert_eq!(backend.calls().len(), 2);
    }
}
