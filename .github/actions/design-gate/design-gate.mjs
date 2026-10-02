#!/usr/bin/env node
/**
 * Design-approved gate for contributor pull requests.
 *
 * The rule: a pull request is reviewable and mergeable only once the issue it
 * links carries a maintainer's `design-approved` label. GitHub has no native
 * "issue first" setting, so the rule is assembled from two primitives — this
 * script's exit code (surfaced as a required status check) and the pull
 * request's draft state.
 *
 * Parsing and decision logic live in this script rather than inline in the
 * workflow so they can be unit tested with no network (design-gate.test.mjs).
 * The workflow only supplies the event payload and the tokens.
 *
 * Two tokens, and which call uses which is the point: the `design-gate` check
 * run goes out on the workflow's own GITHUB_TOKEN (CHECK_TOKEN), everything
 * else on the app token (GITHUB_TOKEN). See `runPullRequestGate`.
 *
 * Usage:
 *   node scripts/design-gate.mjs pull-request   # decide, comment, exit 0/1
 *   node scripts/design-gate.mjs issue-labeled  # release drafts waiting on an issue
 */

import { appendFileSync, readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

export const DESIGN_APPROVED_LABEL = "design-approved";
export const TRIVIAL_LABEL = "trivial";

const REQUIRED_LABELS = [
  {
    name: DESIGN_APPROVED_LABEL,
    color: "0E8A16",
    description: "Design agreed by a maintainer; a PR referencing this issue can be reviewed",
  },
  {
    name: TRIVIAL_LABEL,
    color: "C2E0C6",
    description: "Typo-class change; exempt from the design-approved gate",
  },
];

/** Leave existing labels untouched; concurrent runs may both see a label missing. */
export async function ensureLabels({ api, log = console }) {
  const missingPermission = [];
  for (const label of REQUIRED_LABELS) {
    if (await api.getLabel(label.name)) {
      log.log?.(`design-gate: label ${label.name} present`);
      continue;
    }
    try {
      await api.createLabel(label);
      log.log?.(`design-gate: label ${label.name} created`);
    } catch (error) {
      if (error.status === 422 && /already[ _]exists/i.test(error.body ?? "")) {
        log.log?.(`design-gate: label ${label.name} present`);
        continue;
      }
      if (error.status === 403 || error.status === 404) {
        log.warn?.(`design-gate: could not create label ${label.name}; CK CI App needs Issues: write permission`);
        missingPermission.push(label.name);
        continue;
      }
      throw error;
    }
  }
  return missingPermission;
}

/** Hidden marker that lets a later run find the comment it already posted. */
export const COMMENT_MARKER = "<!-- design-gate -->";

/**
 * Second hidden marker, carried in the same gate comment, recording that the
 * gate itself pushed this pull request back to draft. Approving the linked
 * issue marks a draft ready only when its comment carries this marker: a draft
 * the author opened or left as a draft on purpose is the author's decision,
 * and the gate should undo only what it did.
 */
export const CONVERTED_MARKER = "<!-- design-gate:converted -->";

/** The name of the check run published from the issue arm. Must match the job id. */
export const CHECK_NAME = "design-gate";

/**
 * Branches maintainers (or this repository's own automation) push directly.
 * These carry no contributor design conversation, so the gate does not apply
 * to them — but only when the head is this repository, so a fork cannot bypass
 * the gate by naming its branch `train/whatever`. `ci/` is the automation
 * prefix: the OpenCode pin bump (`.github/workflows/bump-opencode.yml`) opens
 * `ci/bump-opencode-<version>` from an app token, and a mechanical version
 * bump has no design to approve.
 */
export const MAINTAINER_BRANCH_PREFIXES = ["train", "alfonso", "ci"];

/**
 * The only text a blocked contributor reads. Keep it exact: it has to explain
 * the rule, the fix, and the escape hatch without any other context.
 */
/*
 * It must not contain any of GitHub's closing keywords (close, fix, resolve and
 * their inflections), not even as ordinary words: telling a contributor to
 * write one makes GitHub close the issue when the pull request merges, before
 * the release that carries the change has shipped. The test suite pins this.
 */
export const GATE_MESSAGE = [
  "This PR needs a linked issue with the `design-approved` label before it can be reviewed or",
  "merged. Link the approved issue with `Approved issue: #<issue>` (or `Refs #<issue>`) in the",
  "description. A maintainer will apply the `design-approved` label on the issue, or `trivial`",
  "on the pull request when there is genuinely no design to agree. The issue stays open until",
  "the change ships; maintainers take care of it then.",
].join(" ");

/**
 * The ways a pull request body links an issue: a keyword, optional colon,
 * whitespace, then either `#N`, `owner/repo#N`, or a full issue URL.
 *
 * Three keyword families count, and only the first one closes anything:
 * - GitHub's closing keywords (`Closes`, `Fixes`, `Resolves` and their
 *   inflections). Contributors keep writing them, so they are still accepted,
 *   but GitHub closes the issue on merge, so the gate never asks for them.
 * - `Approved issue:`, the line the pull request template carries.
 * - `Ref`/`Refs` and `Part of`, for a pull request that is one step of an
 *   issue and must leave it open. `See #N` is deliberately not a link: it is how
 *   prose mentions an issue, and with the first link winning it could select an
 *   unrelated approved issue.
 */
const ISSUE_LINK_PATTERN = new RegExp(
  [
    String.raw`\b(?:(?<closing>close[sd]?|fix(?:e[sd])?|resolve[sd]?)|(?<approved>approved\s+issue)|(?<reference>refs?|part\s+of))\b\s*:?\s+`,
    "(?:",
    String.raw`https?://github\.com/(?<urlOwner>[\w.-]+)/(?<urlRepo>[\w.-]+)/issues/(?<urlNumber>\d+)`,
    String.raw`|(?:(?<refOwner>[\w.-]+)/(?<refRepo>[\w.-]+))?#(?<refNumber>\d+)`,
    ")",
  ].join(""),
  "gi",
);

/**
 * GitHub does not turn references inside HTML comments, fenced blocks, or code
 * spans into links, so neither does the gate. Pull request templates routinely
 * carry an example `Approved issue: #` inside an HTML comment; that must not count as a
 * real link.
 */
function stripUnlinkedRegions(body) {
  return body
    .replace(/<!--[\s\S]*?-->/g, " ")
    .replace(/```[\s\S]*?```/g, " ")
    .replace(/`[^`\n]*`/g, " ");
}

function sameRepo(a, b) {
  return Boolean(a) && Boolean(b) && a.toLowerCase() === b.toLowerCase();
}

/**
 * First issue this body links in `repoFullName`, or null.
 *
 * References to other repositories are not links here at all — they are
 * skipped, and a later same-repo reference still wins.
 *
 * `kind` says which keyword family matched: `closing` (GitHub will close the
 * issue on merge), `approved` (the template's `Approved issue:` line) or
 * `reference` (`Refs` / `Part of`). The gate treats all three the same.
 *
 * @returns {{ number: number, text: string, kind: "closing" | "approved" | "reference" } | null}
 */
export function parseLinkedIssue(body, repoFullName) {
  if (!body) return null;
  const haystack = stripUnlinkedRegions(body);
  ISSUE_LINK_PATTERN.lastIndex = 0;
  for (const match of haystack.matchAll(ISSUE_LINK_PATTERN)) {
    const groups = match.groups ?? {};
    const owner = groups.urlOwner ?? groups.refOwner;
    const repo = groups.urlRepo ?? groups.refRepo;
    const number = groups.urlNumber ?? groups.refNumber;
    if (owner && !sameRepo(`${owner}/${repo}`, repoFullName)) continue;
    const kind = groups.closing ? "closing" : groups.approved ? "approved" : "reference";
    return { number: Number.parseInt(number, 10), text: match[0].trim(), kind };
  }
  return null;
}

/** `train/**` semantics: everything under the prefix, but not the bare name. */
function isUnderBranchPrefix(ref, prefix) {
  return ref.startsWith(`${prefix}/`) && ref.length > prefix.length + 1;
}

function hasLabel(labels, name) {
  return (labels ?? []).some((label) => label.toLowerCase() === name);
}

/**
 * Why this pull request is not a contributor PR the gate applies to, or null
 * when the gate applies.
 */
export function pullRequestSkipReason(pullRequest, repoFullName) {
  if (hasLabel(pullRequest.labels, TRIVIAL_LABEL)) {
    return `the \`${TRIVIAL_LABEL}\` label is applied`;
  }
  if (sameRepo(pullRequest.headRepoFullName, repoFullName)) {
    const prefix = MAINTAINER_BRANCH_PREFIXES.find((candidate) =>
      isUnderBranchPrefix(pullRequest.headRef ?? "", candidate),
    );
    if (prefix) return `\`${pullRequest.headRef}\` is a maintainer \`${prefix}/**\` branch`;
  }
  return null;
}

/**
 * The whole decision, as a pure function of the pull request, the issue it
 * links (already fetched; null when there is no link or the issue is
 * unreadable), and the event action.
 *
 * @returns {{
 *   conclusion: "success" | "failure",
 *   title: string,
 *   message: string,
 *   linkedIssue: number | null,
 *   convertToDraft: boolean,
 *   skipped: boolean,
 * }}
 */
export function decide({ pullRequest, repoFullName, issue = null, action = "opened" }) {
  const skipReason = pullRequestSkipReason(pullRequest, repoFullName);
  if (skipReason) {
    return {
      conclusion: "success",
      title: "Gate does not apply",
      message: `Design gate skipped: ${skipReason}.`,
      linkedIssue: null,
      convertToDraft: false,
      skipped: true,
    };
  }

  const linked = parseLinkedIssue(pullRequest.body, repoFullName);
  const fail = (title, message) => ({
    conclusion: "failure",
    title,
    message,
    linkedIssue: linked?.number ?? null,
    // `opened` and `ready_for_review` are the two moments a pull request
    // ENTERS the ready state, and a pull request that enters ready without an
    // approved issue should not be ready. `synchronize`, `edited` and
    // `reopened` are moments an already-ready pull request changes; pushing it
    // back to draft there would yank the author out from under an in-flight
    // review, so those comment only.
    //
    // `opened` matters most where there are no required status checks at all:
    // draft conversion is then the only enforcement the gate has, and a
    // contributor who opens a non-draft pull request with no linked issue
    // would otherwise stay ready for review until a human drafted it by hand.
    //
    // `labeled` and `unlabeled` are also comment-only: the caller triggers on
    // them so that a maintainer applying `trivial` re-runs the gate, and a
    // label change is not the pull request entering the ready state.
    convertToDraft: action === "opened" || action === "ready_for_review",
    skipped: false,
  });

  if (!linked) return fail("No linked issue", GATE_MESSAGE);
  if (!issue) return fail(`Linked issue #${linked.number} is unreadable`, GATE_MESSAGE);
  // GitHub's issues endpoint also returns pull requests. Approval belongs to
  // an open design issue, not to a PR or an already-completed discussion.
  if (issue.isPullRequest) {
    return fail(`#${issue.number} is a pull request, not a design issue`, GATE_MESSAGE);
  }
  if (issue.state !== "open") {
    return fail(`Linked issue #${issue.number} is not open`, GATE_MESSAGE);
  }
  if (!hasLabel(issue.labels, DESIGN_APPROVED_LABEL)) {
    return fail(
      `Waiting for \`${DESIGN_APPROVED_LABEL}\` on #${issue.number}`,
      `Waiting for \`${DESIGN_APPROVED_LABEL}\` on #${issue.number}.\n\n${GATE_MESSAGE}`,
    );
  }

  return {
    conclusion: "success",
    title: `#${issue.number} is ${DESIGN_APPROVED_LABEL}`,
    message: `#${issue.number} has the \`${DESIGN_APPROVED_LABEL}\` label; this PR can be reviewed.`,
    linkedIssue: issue.number,
    convertToDraft: false,
    skipped: false,
  };
}

/** The note appended when the gate pushed a ready pull request back to draft. */
export function draftNote(linkedIssue) {
  return linkedIssue === null
    ? "Converted back to draft; it will be marked ready automatically once a linked issue is design-approved."
    : `Converted back to draft; it will be marked ready automatically once #${linkedIssue} is design-approved.`;
}

/**
 * Comment body for a decision, or null when no comment should exist yet.
 *
 * `draftConverted` means "this pull request is a draft the gate is holding":
 * either the gate converted it on this run, or an earlier run did and it is
 * still a draft. The body then carries `CONVERTED_MARKER`, which is the only
 * record the issue arm consults before marking a draft ready. The caller has
 * to pass it on every run while the hold lasts, because the body is rebuilt
 * from scratch each time and a run that dropped the marker would leave the
 * draft stranded after approval.
 */
export function buildCommentBody(decision, { draftConverted = false, commentExists = false, labelWarning = null } = {}) {
  if (decision.conclusion === "success") {
    // Never open a conversation on a passing PR — only close the one already
    // there, so the author is not left reading a stale blocker. The draft
    // note would be stale here too, but the marker stays while the gate still
    // holds the draft.
    if (!commentExists) return null;
    const parts = [COMMENT_MARKER];
    if (draftConverted) parts.push(CONVERTED_MARKER);
    parts.push(decision.message);
    if (labelWarning) parts.push(labelWarning);
    return parts.join("\n\n");
  }
  const parts = [COMMENT_MARKER];
  if (draftConverted) parts.push(CONVERTED_MARKER);
  parts.push(decision.message);
  if (draftConverted) parts.push(draftNote(decision.linkedIssue));
  if (labelWarning) parts.push(labelWarning);
  return parts.join("\n\n");
}

/** Whether a gate comment records that the gate converted its pull request to draft. */
export function carriesConvertedMarker(comment) {
  return Boolean(comment && (comment.body ?? "").includes(CONVERTED_MARKER));
}

/** Fetch the linked issue (if any) and decide. */
export async function evaluatePullRequest({ api, repoFullName, pullRequest, action = "opened" }) {
  const linked = pullRequestSkipReason(pullRequest, repoFullName)
    ? null
    : parseLinkedIssue(pullRequest.body, repoFullName);
  const issue = linked ? await api.getIssue(linked.number) : null;
  return decide({ pullRequest, repoFullName, issue, action });
}

/** The gate comment already on a pull request, or null. Throws when comments cannot be read. */
async function findGateComment({ api, pullRequest }) {
  const comments = await api.listComments(pullRequest.number);
  return comments.find((comment) => (comment.body ?? "").includes(COMMENT_MARKER)) ?? null;
}

/**
 * Keep exactly one gate comment per pull request: create it the first time the
 * gate blocks, edit that same comment on every later run.
 *
 * `existing` is the gate comment as read earlier in the same run (null when
 * there is none), so the caller can act on its markers before it is rewritten.
 * `draftConverted` is whether the rewritten body should keep recording a
 * gate-held draft; see `buildCommentBody`.
 */
async function syncComment({ api, pullRequest, decision, existing, draftConverted = false, labelWarning = null }) {
  const body = buildCommentBody(decision, { draftConverted, commentExists: Boolean(existing), labelWarning });
  if (body === null) return { action: "none" };
  if (!existing) {
    await api.createComment(pullRequest.number, body);
    return { action: "created" };
  }
  if (existing.body === body) return { action: "unchanged" };
  await api.updateComment(existing.id, body);
  return { action: "updated" };
}

/**
 * Run the gate for one pull request event: decide, push a newly-ready PR back
 * to draft when it fails, publish the gate's check run, and keep the single
 * gate comment in sync.
 *
 * Draft conversion and commenting are best-effort — a fork pull request runs
 * with a read-only token and both will be refused. The returned conclusion
 * never depends on them: a side step that fails must not turn a failing gate
 * green.
 *
 * Publishing the check run is NOT best-effort, because it is the gate's
 * output rather than a side effect of it. Branch protection requires the
 * check named `CHECK_NAME`, and that name belongs to this script: the job
 * status a workflow reports is named after whatever the calling repository
 * happened to call its job, so making that the required name would put a
 * caller's job id into the branch-protection contract. A sha that never
 * receives this check run is blocked with nothing in the run list to explain
 * it, so a refused publish has to be loud.
 *
 * That is why the check run goes out on `checkApi` and nothing else does.
 * `checkApi` is built from the workflow's own token, which the calling job
 * grants `checks: write` in a file anyone can read; `api` is built from an
 * app token, whose installation permissions can only be read back with the
 * app's own credentials. The step that decides whether a pull request is
 * blocked must not depend on a permission nobody running the gate can check.
 * Both clients are passed in rather than chosen here, so this function stays
 * free of tokens: the split lives at the edge, in `main`.
 */
export async function runPullRequestGate({
  api,
  checkApi,
  repoFullName,
  pullRequest,
  action = "opened",
  log = console,
}) {
  const missingPermission = await ensureLabels({ api, log });
  const labelWarning = missingPermission.length
    ? `Gate labels (${missingPermission.map((name) => `\`${name}\``).join(", ")}) could not be created. A maintainer must create them manually or grant the CK CI App Issues: write permission.`
    : null;
  const decision = await evaluatePullRequest({ api, repoFullName, pullRequest, action });

  let draftConverted = false;
  if (decision.convertToDraft && !pullRequest.isDraft) {
    try {
      await api.convertPullRequestToDraft(pullRequest.nodeId);
      draftConverted = true;
    } catch (error) {
      log.warn?.(`design-gate: could not convert #${pullRequest.number} to draft: ${error}`);
    }
  }

  // The check run reports the same pass-or-block decision the contributor
  // reads in the gate comment on the pull request. A check run that concluded
  // differently from the comment would tell the contributor the gate passed
  // while the comment told them it blocked, which is worse than no check run.
  await checkApi.createCheckRun({
    headSha: pullRequest.headSha,
    conclusion: decision.conclusion,
    title: decision.title,
    summary: decision.message,
  });

  let comment = { action: "none" };
  try {
    const existing = await findGateComment({ api, pullRequest });
    // The converted marker is sticky while the pull request stays a draft, so
    // a later `synchronize` or `edited` run does not erase the record the
    // issue arm needs. Once the pull request is seen out of draft (the author
    // or a maintainer marked it ready), the hold is over and the marker goes:
    // a later draft is the author's own, not the gate's.
    const gateHeld =
      draftConverted || (pullRequest.isDraft && carriesConvertedMarker(existing));
    comment = await syncComment({
      api,
      pullRequest,
      decision,
      existing,
      draftConverted: gateHeld,
      labelWarning,
    });
  } catch (error) {
    log.warn?.(`design-gate: could not post the gate comment on #${pullRequest.number}: ${error}`);
  }

  return { ...decision, draftConverted, comment };
}

/**
 * A maintainer labelled an issue `design-approved`: release the draft pull
 * requests the gate itself converted while they waited on it, and re-evaluate
 * every open pull request (draft or not) linking this issue.
 *
 * Only drafts whose gate comment carries `CONVERTED_MARKER` are marked ready.
 * A draft the author opened as a draft, or left as one on purpose (say, to ask
 * for design guidance), is the author's own decision, and approval must not
 * flip it to ready under them. Such a draft still gets the green check and the
 * updated comment. When the comments cannot be read, nothing is marked ready:
 * the gate cannot tell whose draft it is, so it fails closed on the flip.
 *
 * Marking a PR ready with GITHUB_TOKEN does not start another workflow run, so
 * this arm also publishes the `design-gate` check run itself. Without that the
 * required check would stay red until the author pushed a commit.
 *
 * The check run goes out on `checkApi` here too, for the same reason it does
 * in `runPullRequestGate`: one rule for where the required check comes from,
 * rather than one per arm. On this arm both clients happen to carry the same
 * token, because an `issues` event needs no app token at all.
 */
export async function runIssueLabeled({ api, checkApi, repoFullName, issue, log = console }) {
  const search = api.searchOpenPullRequests
    ? (number) => api.searchOpenPullRequests(number)
    : (number) => api.searchDraftPullRequests(number);
  const candidates = await search(issue.number);
  const released = [];
  const updated = [];

  for (const number of candidates) {
    const pullRequest = await api.getPullRequest(number);
    if (!pullRequest || pullRequest.state !== "open") continue;

    // The search index matches the raw string `#N` anywhere in the body, so
    // re-parse with the real grammar before touching anything.
    const linked = parseLinkedIssue(pullRequest.body, repoFullName);
    if (!linked || linked.number !== issue.number) continue;

    const decision = decide({ pullRequest, repoFullName, issue, action: "labeled" });
    if (decision.conclusion !== "success") {
      log.warn?.(`design-gate: #${number} still fails the gate`);
      continue;
    }

    let existing = null;
    let commentReadable = true;
    try {
      existing = await findGateComment({ api, pullRequest });
    } catch (error) {
      commentReadable = false;
      log.warn?.(
        `design-gate: could not read the gate comment on #${number}; leaving its draft state alone: ${error}`,
      );
    }

    if (commentReadable && pullRequest.isDraft && carriesConvertedMarker(existing)) {
      await api.markPullRequestReadyForReview(pullRequest.nodeId);
      released.push(number);
    }
    await checkApi.createCheckRun({
      headSha: pullRequest.headSha,
      conclusion: "success",
      title: decision.title,
      summary: decision.message,
    });
    // Rewritten without the converted marker in every case: a released draft
    // is no longer held, and a pull request that was not released either was
    // never held or is no longer a draft.
    if (commentReadable) {
      try {
        await syncComment({ api, pullRequest, decision, existing });
      } catch (error) {
        log.warn?.(`design-gate: could not update the gate comment on #${number}: ${error}`);
      }
    }
    updated.push(number);
  }

  return { released, updated };
}

export function normalizePullRequest(raw) {
  return {
    number: raw.number,
    body: raw.body ?? "",
    nodeId: raw.node_id,
    state: raw.state ?? "open",
    isDraft: Boolean(raw.draft),
    labels: (raw.labels ?? []).map((label) => (typeof label === "string" ? label : label.name)),
    headRef: raw.head?.ref ?? "",
    headRepoFullName: raw.head?.repo?.full_name ?? "",
    headSha: raw.head?.sha ?? "",
  };
}

export function normalizeIssue(raw) {
  return {
    number: raw.number,
    state: raw.state ?? "open",
    isPullRequest: Boolean(raw.pull_request),
    labels: (raw.labels ?? []).map((label) => (typeof label === "string" ? label : label.name)),
  };
}

/** REST + GraphQL client, narrowed to exactly the calls the gate makes. */
export function createGitHubApi({
  token,
  repoFullName,
  apiBase = process.env.GITHUB_API_URL || "https://api.github.com",
  graphqlUrl = process.env.GITHUB_GRAPHQL_URL || "https://api.github.com/graphql",
  fetchImpl = fetch,
}) {
  const headers = {
    accept: "application/vnd.github+json",
    authorization: `Bearer ${token}`,
    "user-agent": "aft-design-gate",
    "x-github-api-version": "2022-11-28",
  };

  async function rest(method, path, body, missingIsError = false) {
    const response = await fetchImpl(`${apiBase}${path}`, {
      method,
      headers: body ? { ...headers, "content-type": "application/json" } : headers,
      body: body ? JSON.stringify(body) : undefined,
    });
    if (response.status === 404 && !missingIsError) return null;
    if (!response.ok) {
      const body = await response.text();
      const error = new Error(`${method} ${path} failed: ${response.status} ${body}`);
      error.status = response.status;
      error.body = body;
      throw error;
    }
    return response.status === 204 ? null : await response.json();
  }

  async function graphql(query, variables) {
    const response = await fetchImpl(graphqlUrl, {
      method: "POST",
      headers: { ...headers, "content-type": "application/json" },
      body: JSON.stringify({ query, variables }),
    });
    const payload = await response.json();
    if (!response.ok || payload.errors) {
      throw new Error(`GraphQL failed: ${response.status} ${JSON.stringify(payload.errors ?? {})}`);
    }
    return payload.data;
  }

  return {
    async getLabel(name) {
      return await rest("GET", `/repos/${repoFullName}/labels/${encodeURIComponent(name)}`);
    },
    async createLabel(label) {
      return await rest("POST", `/repos/${repoFullName}/labels`, label, true);
    },
    async getIssue(number) {
      const raw = await rest("GET", `/repos/${repoFullName}/issues/${number}`);
      return raw ? normalizeIssue(raw) : null;
    },
    async getPullRequest(number) {
      const raw = await rest("GET", `/repos/${repoFullName}/pulls/${number}`);
      return raw ? normalizePullRequest(raw) : null;
    },
    async listComments(number) {
      const comments = [];
      // Pull request review threads are separate; the gate comment is a plain
      // issue comment, so one paginated pass over that list finds it.
      for (let page = 1; page <= 5; page += 1) {
        const batch = await rest(
          "GET",
          `/repos/${repoFullName}/issues/${number}/comments?per_page=100&page=${page}`,
        );
        if (!batch?.length) break;
        comments.push(...batch);
        if (batch.length < 100) break;
      }
      return comments;
    },
    async createComment(number, body) {
      return await rest("POST", `/repos/${repoFullName}/issues/${number}/comments`, { body });
    },
    async updateComment(id, body) {
      return await rest("PATCH", `/repos/${repoFullName}/issues/comments/${id}`, { body });
    },
    async searchOpenPullRequests(issueNumber) {
      const query = `is:pr is:open "#${issueNumber}" repo:${repoFullName}`;
      const result = await rest(
        "GET",
        `/search/issues?per_page=100&q=${encodeURIComponent(query)}`,
      );
      return (result?.items ?? []).map((item) => item.number);
    },
    async searchDraftPullRequests(issueNumber) {
      return this.searchOpenPullRequests(issueNumber);
    },
    async convertPullRequestToDraft(nodeId) {
      // REST cannot move a pull request back to draft; only GraphQL can.
      await graphql(
        "mutation($id: ID!) { convertPullRequestToDraft(input: { pullRequestId: $id }) { pullRequest { isDraft } } }",
        { id: nodeId },
      );
    },
    async markPullRequestReadyForReview(nodeId) {
      await graphql(
        "mutation($id: ID!) { markPullRequestReadyForReview(input: { pullRequestId: $id }) { pullRequest { isDraft } } }",
        { id: nodeId },
      );
    },
    async createCheckRun({ headSha, conclusion, title, summary }) {
      return await rest("POST", `/repos/${repoFullName}/check-runs`, {
        name: CHECK_NAME,
        head_sha: headSha,
        status: "completed",
        conclusion,
        output: { title, summary },
      });
    },
  };
}

function requireEnv(name) {
  const value = process.env[name];
  if (!value) throw new Error(`design-gate: ${name} is not set`);
  return value;
}

function writeStepSummary(text) {
  const path = process.env.GITHUB_STEP_SUMMARY;
  if (path) appendFileSync(path, `${text}\n`);
}

function annotateError(message) {
  const escaped = message.replace(/%/g, "%25").replace(/\r/g, "%0D").replace(/\n/g, "%0A");
  console.log(`::error title=${CHECK_NAME}::${escaped}`);
}

async function main(argv) {
  const mode = argv[0];
  const repoFullName = requireEnv("GITHUB_REPOSITORY");
  const event = JSON.parse(readFileSync(requireEnv("GITHUB_EVENT_PATH"), "utf8"));
  // One client per token, built here and nowhere else, so the rest of the
  // script never sees a token. `api` carries the app token: the comment, the
  // draft conversion and the ready-for-review go out on it. `checkApi`
  // carries the calling job's own GITHUB_TOKEN and publishes the check run
  // branch protection reads, so that the blocking path needs no permission
  // from outside the workflow. Both are required: a missing one throws here,
  // before any decision is made, rather than quietly sending the check run
  // out on whichever token happened to be set.
  const api = createGitHubApi({ token: requireEnv("GITHUB_TOKEN"), repoFullName });
  const checkApi = createGitHubApi({ token: requireEnv("CHECK_TOKEN"), repoFullName });

  if (mode === "pull-request") {
    const pullRequest = normalizePullRequest(event.pull_request);
    const result = await runPullRequestGate({
      api,
      checkApi,
      repoFullName,
      pullRequest,
      action: event.action,
    });
    // The job summary is what GitHub shows as the check's summary, so the
    // contributor reads the same words in the checks tab and in the comment.
    writeStepSummary(result.message);
    console.log(`design-gate: #${pullRequest.number} -> ${result.conclusion} (${result.title})`);
    if (result.conclusion !== "success") {
      annotateError(result.message);
      process.exitCode = 1;
    }
    return;
  }

  if (mode === "issue-labeled") {
    const issue = normalizeIssue(event.issue);
    const { released, updated = [] } = await runIssueLabeled({
      api,
      checkApi,
      repoFullName,
      issue,
    });
    const parts = [];
    if (released.length) {
      parts.push(
        `Marked ready for review after #${issue.number} was design-approved: ${released
          .map((number) => `#${number}`)
          .join(", ")}`,
      );
    }
    const readyUpdated = updated.filter((number) => !released.includes(number));
    if (readyUpdated.length) {
      parts.push(
        `Published check for open pull requests after #${issue.number} was design-approved: ${readyUpdated
          .map((number) => `#${number}`)
          .join(", ")}`,
      );
    }
    const summary = parts.length
      ? parts.join("\n")
      : `No pull requests were waiting on #${issue.number}.`;
    writeStepSummary(summary);
    console.log(`design-gate: ${summary}`);
    return;
  }

  throw new Error(`design-gate: unknown mode ${JSON.stringify(mode)}`);
}

const invokedPath = process.argv[1] ? pathToFileURL(process.argv[1]).href : "";
if (invokedPath === import.meta.url) {
  main(process.argv.slice(2)).catch((error) => {
    console.error(`design-gate: ${error instanceof Error ? error.stack : error}`);
    process.exitCode = 1;
  });
}
