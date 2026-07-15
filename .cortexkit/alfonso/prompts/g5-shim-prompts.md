# Task: G5 shim lane — MCP Prompts scaffolding in subc-mcp (session-neutral, adapter-gated dispatch)

Repo: ~/Work/Projects/CortexKit/subconscious, master HEAD. Scope: crates/subc-mcp only. Do NOT use any subagents. No deploy; nothing here changes prod behavior until a policy/dispatch wiring lands later.

GOAL: advertise MCP Prompts from the shim and implement prompts/list + prompts/get for two prompts, `wrapup` and `status`, with the actual backend dispatch isolated behind an adapter trait whose real implementations arrive later (op shapes are being designed by another module owner). Everything you build must be session-neutral: prompts/get must NOT resolve, look up, or mutate any session/lineage state.

## Architecture facts (verified at source — trust these)
- The MCP server is impl ServerHandler for SubcMcpServer in crates/subc-mcp/src/main.rs (~line 3300), rmcp 1.7.0.
- rmcp's ServerHandler trait already has list_prompts/get_prompt default methods and the types (ListPromptsResult, GetPromptResult, PromptMessage, Prompt, PromptArgument, GetPromptRequestParams) — check rmcp 1.7.0 docs/source for exact names. Capabilities: add .enable_prompts() to the ServerCapabilities builder in get_info().
- The shim's launch identity is CK_INSTANCE_TOKEN (instance_token_from_env(), used in ShimHello ~line 1519). The adapter will need it later; plumb the Option<String> instance token INTO the adapter call, nothing else.

## Deliverables

1. CAPABILITY: get_info() advertises prompts (enable_prompts + keep existing tools capability unchanged).

2. PROMPTS SURFACE (exact, closed set):
   - `status`: description "Summarize the current conversation state from Magic Context." No arguments.
   - `wrapup`: description "Wrap up this conversation: fold history and keep only the most recent messages." One OPTIONAL argument `keep`: "number of recent messages to keep (5-100, default 20)".
   - prompts/list returns exactly these two descriptors (names, descriptions, argument declarations) — write a test asserting the EXACT serialized descriptor set so any drift is a test failure.

3. KEEP PARSING (strict): `keep` arrives as an MCP prompt argument (string). Parse: must be an integer, default 20 when absent, clamp is NOT applied — out-of-bounds is an ERROR: values outside 5..=100 return an invalid-params error naming the bound. Non-integer/malformed → invalid-params error. Duplicate/unknown argument names → invalid-params error. (Strict parse + bounds per the cross-module contract; default 20.)

4. ADAPTER SEAM (the important structure): define a trait, e.g.
   ```rust
   #[async_trait]
   trait PromptBackend: Send + Sync {
       async fn status(&self, instance_token: Option<&str>) -> Result<String, PromptBackendError>;
       async fn enqueue_wrapup(&self, instance_token: Option<&str>, keep: u32) -> Result<String, PromptBackendError>;
   }
   ```
   prompts/get parses/validates, then calls the adapter, and wraps the returned string as the prompt text (single user PromptMessage). The ONLY shipped implementation for now is a `PendingBackend` that returns a typed unavailable error ("wrapup/status backend not wired yet") — the real MC/Thalamus clients arrive when their op shapes are finalized. PromptBackendError maps to a clean MCP error (unavailable vs invalid-params vs internal), never a panic.
   - Wire the adapter so tests can inject a mock backend (constructor takes Arc<dyn PromptBackend>).

5. MUTATION-SENSITIVE PROTOCOL TESTS (rmcp-level where the existing test harness allows; follow the crate's existing test patterns):
   - prompts/list exact descriptor set (names/descriptions/args, serialized compare).
   - prompts/get unknown prompt name → error.
   - keep: absent→20, "5"→5, "100"→100, "4"→bounds error, "101"→bounds error, "abc"→invalid-params, duplicate arg→invalid-params.
   - backend failure (mock returning error) → clean MCP error, no panic.
   - backend success (mock returning "SUMMARY") → GetPromptResult carries exactly one user message with that text.
   - retry/idempotence seam: calling prompts/get twice with the same args calls the adapter twice (no shim-side caching) — assert via mock call count.
   - EXISTING SURFACE UNAFFECTED: tools/list and tools/call behavior unchanged (run the existing subc-mcp test suite; add one test asserting tools capability still advertised alongside prompts).
   - The instance token is passed through to the adapter verbatim (mock captures it).

6. NO session/lineage calls anywhere in this slice: no route.open to magic-context, no session.resolve, nothing stateful. The PendingBackend proves it structurally (grep-level: prompts code paths must not reference the relay/route machinery).

GREEN BAR: env -u SUBC_MODULE_ID -u SUBC_LAUNCH_NONCE cargo test -p subc-mcp green; clippy native + --target x86_64-pc-windows-gnu clean; fmt clean; check_comments clean (comments explain design for a no-context reader; NO em dashes anywhere).

REPORT: files changed, the exact prompts/list JSON as serialized, test list with the descriptor-drift test called out, commit SHA.