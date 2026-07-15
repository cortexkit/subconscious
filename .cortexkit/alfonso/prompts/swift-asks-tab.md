# Task: Add an "Asks" tab to the SubcChat Swift app

Repo: ~/Work/Projects/CortexKit/subconscious (work off master HEAD ≥ b61f6c1d).
Scope: clients/subc-client-swift ONLY. Do NOT touch other crates/packages. Do NOT spawn or use any subagents.

GOAL: Ufuk answers pending Alfonso "asks" (agent questions parked for the user) from the app. A new top-level tab "Asks" alongside the existing Chat / Rooms / Observe tabs, following the EXISTING Observe pattern exactly (poll → list → detail → action). The wire contract below is authoritative, verified at source by the alfonso-core owner. Mirror its casing exactly.

## Follow the existing patterns — read these first
- Sources/SubcChat/ObserveViewModel.swift — THE pattern to copy: @MainActor ObservableObject, private DispatchQueue work queue, SubcClient.connect(connectionFilePath:), routeOpenManagementSurface to module "alfonso-core" (same target/args as ensureAlfonsoBlocking there), callManagement(route:method:params:), JSONKeyNormalizer camelize, decode<T: Decodable> via JSONSerialization round-trip, Timer polling with appear()/disappear(), status string for errors (shortError helper).
- Sources/SubcChat/ObserveView.swift + ObserveModels.swift — list/detail SwiftUI layout conventions, row styling, badge patterns.
- Sources/SubcChat/ContentView.swift — how tabs are declared; add the Asks tab there.
- Identity: reuse the same harness ("ck-app") + minted session-id + callerDirectory approach ObserveViewModel uses.

## Wire contract (alfonso-core management surface — casing EXACT)

1. LIST: method "ask.list_pending_for_user", params {} (NO filter — fleet-wide pending user-asks). Returns AskRequest[] ordered askedAt ASC (the reply may wrap rows under a key like the observe ops — use the same rowsArray unwrap tolerance as ObserveViewModel). Poll every 5s while the tab is visible.
2. READ ONE: "ask.get" {requestID} → AskRequest | null. (Use for detail refresh after actions.)
3. ANSWER: "ask.persist_answer" {requestID, answer: String}. Reply outcomes to handle EXPLICITLY:
   - {ok:true, alreadyAnswered:false, request} → answered; show confirmation, refresh list.
   - {ok:true, alreadyAnswered:true, request} → same-text replay; treat as answered.
   - {ok:false, code:"conflict", request} → NORMAL outcome, NOT an error: render "Answered elsewhere or auto-proceeded" with the returned request's recorded answer/state as truth.
   - {ok:false, code:"canceled", request} → "Ask was canceled by the asker."
   - {ok:false, code:"not_found"} → "Ask no longer exists"; drop from list.
4. DISMISS: "ask.resolve_user_ask" {requestID, askerSessionID (copy from the ask record — must match), resolution?: String}. Expose as a "Dismiss" action with an optional short resolution text ("what was decided").

## AskRequest fields (camelCase; absent = not provided)
requestID, purpose ("general"|"campaign_approval"), recipientKind, askerSessionID, taskID?, question (required), context?, whyItMatters?, reversibility? (Double 0..1), scope?, materialDamage? (Bool), refs? ([String]), defaultDecision?, options? ([{label, description?, tradeoff?, recommended?}]), answerKind, urgency ("low"|"normal"|"high"), blocking (Bool), askedAt (epoch ms), silencePolicy? ({mode:"fyi"|"veto_window"|"block", waitUntil?: epochMs, effectiveAutonomy}).
Decode defensively like ObserveModels does (optionals everywhere except requestID/question/askedAt; tolerate unknown enum strings by keeping the raw string).

## UI requirements

LIST (left column, like Observe):
- Row: urgency badge (high=red, normal=gray, low=muted), a distinct MATERIAL-DAMAGE badge when materialDamage==true (e.g. orange "material" tag — must be prominent), question (2-line preview), asker session short-form (last 6 chars is fine) + relative age from askedAt, and a small countdown chip when silencePolicy.mode=="veto_window" with waitUntil in the future ("auto-proceeds in 12m").
- Sort: askedAt ASC (server order). Tab label shows pending count when > 0 (e.g. "Asks (3)").

DETAIL (right pane on selection):
- Question prominent. Then, when present: context, whyItMatters, scope, refs (monospace list), reversibility (render as e.g. "reversibility 0.3" with a subtle bar), blocking, taskID, asker session, askedAt absolute+relative.
- defaultDecision shown as "If unanswered: <text>" — with the veto_window countdown next to it when applicable ("auto-proceeds at <time>"); if waitUntil already passed, show "may have auto-proceeded" (an answer attempt will surface conflict truth).
- OPTIONS: buttons, one per option, label + description + tradeoff beneath; recommended==true visually highlighted and listed first only if server order doesn't already do it (preserve server order, just highlight). Clicking an option sends its LABEL VERBATIM via persist_answer.
- FREE TEXT: a text field + Send button (multi-line ok) for non-option answers.
- CAMPAIGN APPROVALS (purpose=="campaign_approval"): the module VALIDATES answers — render Approve and Reject buttons (sending "approve" / "reject") INSTEAD of the free-text-first layout; keep an "Amend (advanced)" disclosure with a raw text area for amend-JSON. Do not send arbitrary free text as the primary action for these.
- DISMISS: secondary action with optional resolution text; uses ask.resolve_user_ask with the record's askerSessionID.
- After any action: refresh via ask.get, render the returned request state as truth, and re-poll the list.

STATES: empty state ("No pending asks"), ops-unavailable state matching Observe's opsAvailable handling (if the list call fails with an unknown-op-shaped error, show the same "ops not available" style message rather than a scary error), transient error banner via status string.

## Build/verify
- swift build from clients/subc-client-swift must compile clean.
- swift test must stay green (existing tests must not break; add decoder unit tests for AskRequest covering: full record, minimal record (only required fields), options with recommended, silencePolicy veto_window, unknown enum values tolerated, epoch-ms date decoding).
- Answer-outcome handling unit-testable where practical: factor the persist_answer reply parsing into a pure function and unit-test all five outcome shapes above (fail-first: assert conflict maps to the answered-elsewhere presentation, NOT an error).
- check_comments before commit; comments explain semantics for a no-context reader (e.g. why conflict is a normal outcome), never reference tasks/sessions/peers.
- Commit locally in the worktree; do not push.

## Report
Files changed, the decoder + outcome-parsing test list with fail-first evidence for the conflict test, swift build/test output, commit SHA.