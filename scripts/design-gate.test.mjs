#!/usr/bin/env node
/**
 * Unit tests for the design-approved gate.
 *
 * Run with: node --test scripts/design-gate.test.mjs
 *
 * Everything here is offline. The GitHub API is a small in-memory fixture, so
 * the suite never touches the real repository — in particular it never creates
 * labels or comments anywhere.
 */

import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { describe, test } from "node:test";
import { fileURLToPath } from "node:url";

import {
  buildCommentBody,
  CHECK_NAME,
  COMMENT_MARKER,
  CONVERTED_MARKER,
  createGitHubApi,
  decide,
  DESIGN_APPROVED_LABEL,
  draftNote,
  GATE_MESSAGE,
  parseLinkedIssue,
  pullRequestSkipReason,
  runIssueLabeled,
  runPullRequestGate,
  TRIVIAL_LABEL,
} from "../.github/actions/design-gate/design-gate.mjs";

const REPO = "cortexkit/aft";

function pullRequest(overrides = {}) {
  return {
    number: 100,
    body: "",
    nodeId: "PR_node_100",
    state: "open",
    isDraft: false,
    labels: [],
    headRef: "feature/thing",
    headRepoFullName: "contributor/aft",
    headSha: "a".repeat(40),
    ...overrides,
  };
}

/**
 * In-memory stand-in for the REST + GraphQL calls the gate makes. `refuse`
 * names methods that should throw, which is how a fork pull request's
 * read-only token behaves.
 *
 * Two clients come back, because the gate is handed two: `api` stands for the
 * app token, which posts the comment and converts to draft, and `checkApi`
 * for the calling job's own GITHUB_TOKEN, which publishes the required check
 * run. They share one state, and every recorded check run carries a `via`
 * naming the client that published it, so an arm can assert which token the
 * check went out on rather than only that some check was published.
 */
function createFixtureApi({
  issues = [], pullRequests = [], comments = [], refuse = [],
  labels = [{ name: DESIGN_APPROVED_LABEL }, { name: TRIVIAL_LABEL }],
  createLabelError = null,
} = {}) {
  const state = {
    issues: new Map(issues.map((issue) => [issue.number, issue])),
    labels: new Map(labels.map((label) => [label.name, label])),
    createdLabels: [],
    pullRequests: new Map(pullRequests.map((pr) => [pr.number, pr])),
    comments: comments.map((comment) => ({ ...comment })),
    checkRuns: [],
    readyForReview: [],
    convertedToDraft: [],
    nextCommentId: 1,
  };

  function guard(name) {
    if (refuse.includes(name)) throw new Error(`fixture: ${name} refused (read-only token)`);
  }

  const api = {
    async getLabel(name) {
      return state.labels.get(name) ?? null;
    },
    async createLabel(label) {
      if (createLabelError) throw createLabelError;
      state.createdLabels.push(label);
      state.labels.set(label.name, label);
      return label;
    },
    async getIssue(number) {
      return state.issues.get(number) ?? null;
    },
    async getPullRequest(number) {
      return state.pullRequests.get(number) ?? null;
    },
    async listComments(number) {
      guard("listComments");
      return state.comments.filter((comment) => comment.issueNumber === number);
    },
    async createComment(number, body) {
      guard("createComment");
      const comment = { id: state.nextCommentId++, issueNumber: number, body };
      state.comments.push(comment);
      return comment;
    },
    async updateComment(id, body) {
      guard("updateComment");
      const comment = state.comments.find((candidate) => candidate.id === id);
      assert.ok(comment, `fixture: no comment ${id}`);
      comment.body = body;
      return comment;
    },
    async searchDraftPullRequests(issueNumber) {
      // The real search matches the literal string anywhere in the body and
      // knows nothing about closing keywords. Reproduce that looseness so the
      // caller's re-parse is actually exercised.
      return [...state.pullRequests.values()]
        .filter((pr) => pr.state === "open" && pr.body.includes(`#${issueNumber}`))
        .map((pr) => pr.number);
    },
    async searchOpenPullRequests(issueNumber) {
      return this.searchDraftPullRequests(issueNumber);
    },
    async convertPullRequestToDraft(nodeId) {
      guard("convertPullRequestToDraft");
      state.convertedToDraft.push(nodeId);
      for (const pr of state.pullRequests.values()) if (pr.nodeId === nodeId) pr.isDraft = true;
    },
    async markPullRequestReadyForReview(nodeId) {
      guard("markPullRequestReadyForReview");
      state.readyForReview.push(nodeId);
      for (const pr of state.pullRequests.values()) if (pr.nodeId === nodeId) pr.isDraft = false;
    },
    async createCheckRun(run) {
      guard("createCheckRun");
      state.checkRuns.push({ ...run, via: "app" });
      return run;
    },
  };

  const checkApi = {
    ...api,
    async createCheckRun(run) {
      guard("createCheckRun");
      state.checkRuns.push({ ...run, via: "check" });
      return run;
    },
  };

  return { api, checkApi, state };
}

const silentLog = { warn() {}, log() {} };

describe("required labels", () => {
  const expected = [
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

  test("a pull request run creates both missing labels before evaluating", async () => {
    const { api, checkApi, state } = createFixtureApi({ labels: [] });
    const lines = [];
    const result = await runPullRequestGate({
      api, checkApi, repoFullName: REPO, pullRequest: pullRequest({ labels: [TRIVIAL_LABEL] }),
      log: { log: (line) => lines.push(line) },
    });
    assert.equal(result.conclusion, "success");
    assert.deepEqual(state.createdLabels, expected);
    assert.deepEqual(lines, [
      "design-gate: label design-approved created",
      "design-gate: label trivial created",
    ]);
  });

  test("a pull request run leaves present labels unchanged without writes", async () => {
    const { api, checkApi, state } = createFixtureApi({ labels: expected });
    const lines = [];
    await runPullRequestGate({
      api, checkApi, repoFullName: REPO, pullRequest: pullRequest({ labels: [TRIVIAL_LABEL] }),
      log: { log: (line) => lines.push(line) },
    });
    assert.deepEqual(state.createdLabels, []);
    assert.deepEqual([...state.labels.values()], expected);
    assert.deepEqual(lines, [
      "design-gate: label design-approved present",
      "design-gate: label trivial present",
    ]);
  });

  test("an already-exists 422 from concurrent creation is treated as present", async () => {
    const error = Object.assign(new Error("already exists"), {
      status: 422, body: '{"errors":[{"code":"already_exists"}]}',
    });
    const { api, checkApi, state } = createFixtureApi({ labels: [], createLabelError: error });
    const lines = [];
    await runPullRequestGate({
      api, checkApi, repoFullName: REPO, pullRequest: pullRequest({ labels: [TRIVIAL_LABEL] }),
      log: { log: (line) => lines.push(line) },
    });
    assert.deepEqual(state.createdLabels, []);
    assert.deepEqual(lines, [
      "design-gate: label design-approved present",
      "design-gate: label trivial present",
    ]);
  });

  for (const status of [403, 404]) {
    test(`a ${status} creating labels warns and still evaluates the PR`, async () => {
      const error = Object.assign(new Error("refused"), { status });
      const { api, checkApi, state } = createFixtureApi({ labels: [], createLabelError: error });
      const warnings = [];
      const result = await runPullRequestGate({
        api, checkApi, repoFullName: REPO, pullRequest: pullRequest(),
        log: { warn: (line) => warnings.push(line) },
      });
      assert.equal(result.conclusion, "failure");
      assert.equal(state.checkRuns[0].conclusion, "failure");
      assert.equal(warnings.length, 2);
      for (const warning of warnings) assert.match(warning, /CK CI App needs Issues: write permission/);
      assert.match(state.comments[0].body, /Gate labels \(`design-approved`, `trivial`\) could not be created/);
      assert.match(state.comments[0].body, /grant the CK CI App Issues: write permission/);
    });
  }

  test("a missing label permission does not block a passing PR", async () => {
    const error = Object.assign(new Error("Forbidden"), { status: 403 });
    const { api, checkApi, state } = createFixtureApi({ labels: [], createLabelError: error });
    const result = await runPullRequestGate({
      api, checkApi, repoFullName: REPO, pullRequest: pullRequest({ labels: [TRIVIAL_LABEL] }),
      log: silentLog,
    });
    assert.equal(result.conclusion, "success");
    assert.equal(state.checkRuns[0].conclusion, "success");
    assert.deepEqual(state.comments, []);
  });

  test("an issue-labeled run does not try to create labels with the workflow token", async () => {
    const { api, checkApi, state } = createFixtureApi({ labels: [], createLabelError: new Error("no App token") });
    await runIssueLabeled({ api, checkApi, repoFullName: REPO, issue: { number: 42 }, log: silentLog });
    assert.deepEqual(state.createdLabels, []);
  });

  test("GitHub client gets labels and creates them with the write token", async () => {
    const calls = [];
    const api = createGitHubApi({
      token: "write-token", repoFullName: REPO,
      fetchImpl: async (url, options) => {
        calls.push({ url, options });
        return new Response(options.method === "GET" ? "" : JSON.stringify(expected[0]), {
          status: options.method === "GET" ? 404 : 201,
        });
      },
    });
    assert.equal(await api.getLabel(DESIGN_APPROVED_LABEL), null);
    await api.createLabel(expected[0]);
    assert.deepEqual(calls.map(({ url, options }) => [options.method, url]), [
      ["GET", `https://api.github.com/repos/${REPO}/labels/design-approved`],
      ["POST", `https://api.github.com/repos/${REPO}/labels`],
    ]);
    assert.equal(calls[1].options.headers.authorization, "Bearer write-token");
    assert.deepEqual(JSON.parse(calls[1].options.body), expected[0]);
  });

  test("GitHub client preserves a 404 refusal on label creation", async () => {
    const api = createGitHubApi({
      token: "write-token", repoFullName: REPO,
      fetchImpl: async () => new Response("Not Found", { status: 404 }),
    });
    assert.equal(await api.getLabel(DESIGN_APPROVED_LABEL), null);
    await assert.rejects(api.createLabel(expected[0]), (error) => error.status === 404);
  });
});

describe("parseLinkedIssue", () => {
  const cases = [
    ["close #12", 12],
    ["closes #12", 12],
    ["closed #12", 12],
    ["fix #12", 12],
    ["fixes #12", 12],
    ["fixed #12", 12],
    ["resolve #12", 12],
    ["resolves #12", 12],
    ["resolved #12", 12],
    ["CLOSES #12", 12],
    ["Closes: #12", 12],
    ["Closes  #12", 12],
    ["Some prose.\n\nCloses #12\n\nMore prose.", 12],
    ["Closes #12.", 12],
    [`Closes https://github.com/${REPO}/issues/12`, 12],
    [`Closes HTTPS://GITHUB.COM/${REPO}/issues/12`, 12],
    [`Closes ${REPO}#12`, 12],
    // Cross-repository references are somebody else's issue tracker: they are
    // not a link for this gate at all.
    ["Closes https://github.com/other/repo/issues/12", null],
    ["Closes other/repo#12", null],
    // Non-closing links: the template's own line, and the forms for a pull
    // request that is one step of an issue and must leave it open.
    ["Approved issue: #12", 12],
    ["approved issue: #12", 12],
    [`Approved issue: ${REPO}#12`, 12],
    [`Approved issue: https://github.com/${REPO}/issues/12`, 12],
    ["Refs #12", 12],
    ["Ref #12", 12],
    ["refs: #12", 12],
    ["Refs #12 (not closing)", 12],
    [`Refs https://github.com/${REPO}/issues/12`, 12],
    ["Part of #12", 12],
    ["PART OF #12", 12],
    ["Refs other/repo#12", null],
    ["Approved issue: https://github.com/other/repo/issues/12", null],
    // Negative control: a bare reference with no closing keyword is a
    // mention, not a link. GitHub does not close it and neither do we.
    ["See #123 for context", null],
    ["see #42 for background", null],
    ["#123", null],
    ["Related to #123", null],
    // Keyword-lookalikes must not match.
    ["Closer #1", null],
    ["Encloses #2", null],
    ["Refixes #3", null],
    // Whitespace between keyword and reference is required.
    ["Closes#12", null],
    ["", null],
    [null, null],
  ];

  for (const [body, expected] of cases) {
    test(`${JSON.stringify(body)} -> ${expected}`, () => {
      const linked = parseLinkedIssue(body, REPO);
      assert.equal(linked?.number ?? null, expected);
    });
  }

  test("takes the first link when several are present", () => {
    assert.equal(parseLinkedIssue("Closes #7\nFixes #9", REPO).number, 7);
  });

  test("skips a cross-repo reference and takes the first same-repo one", () => {
    const body = "Closes https://github.com/other/repo/issues/1\nFixes #9";
    assert.equal(parseLinkedIssue(body, REPO).number, 9);
  });

  test("skips a cross-repo Refs and takes the first same-repo link", () => {
    const linked = parseLinkedIssue("Refs cortexkit/other#1\nRefs #9", REPO);
    assert.equal(linked.number, 9);
    assert.equal(linked.kind, "reference");
  });

  test("the first link wins across keyword families", () => {
    assert.equal(parseLinkedIssue("Refs #7\nCloses #9", REPO).number, 7);
    assert.equal(parseLinkedIssue("Closes #9\nApproved issue: #7", REPO).number, 9);
  });

  test("reports which keyword family matched", () => {
    assert.equal(parseLinkedIssue("Approved issue: #1", REPO).kind, "approved");
    assert.equal(parseLinkedIssue("Refs #1", REPO).kind, "reference");
    assert.equal(parseLinkedIssue("Part of #1", REPO).kind, "reference");
    assert.equal(parseLinkedIssue("Closes #1", REPO).kind, "closing");
  });

  test("an unfilled pull request template does not link", () => {
    // Keep in step with .github/pull_request_template.md: a template nobody
    // filled in must fail the gate, not silently satisfy it.
    const template = readFileSync(
      new URL("../.github/pull_request_template.md", import.meta.url),
      "utf8",
    );
    assert.equal(parseLinkedIssue(template, REPO), null);
    const filled = template.replace("Approved issue: #", "Approved issue: #42");
    assert.notEqual(filled, template, "template no longer carries an `Approved issue: #` line");
    assert.equal(parseLinkedIssue(filled, REPO).number, 42);
  });

  test("ignores references GitHub would not linkify", () => {
    assert.equal(parseLinkedIssue("<!-- Closes #5 -->", REPO), null);
    assert.equal(parseLinkedIssue("<!-- Closes #5 -->\nCloses #6", REPO).number, 6);
    assert.equal(parseLinkedIssue("Write `Closes #5` in the description", REPO), null);
    assert.equal(parseLinkedIssue("```\nCloses #5\n```", REPO), null);
  });

  test("ignores a Refs link GitHub would not linkify", () => {
    assert.equal(parseLinkedIssue("<!-- Refs #5 -->", REPO), null);
    assert.equal(parseLinkedIssue("Write `Refs #5` in the description", REPO), null);
    assert.equal(parseLinkedIssue("```\nRefs #5\n```", REPO), null);
    assert.equal(parseLinkedIssue("<!-- Refs #5 -->\nRefs #6", REPO).number, 6);
  });
});

describe("pullRequestSkipReason", () => {
  const cases = [
    ["maintainer train branch", { headRepoFullName: REPO, headRef: "train/v0.56" }, true],
    ["maintainer alfonso branch", { headRepoFullName: REPO, headRef: "alfonso/task/abc" }, true],
    ["nested train branch", { headRepoFullName: REPO, headRef: "train/a/b/c" }, true],
    // The pin-bump automation opens `ci/bump-opencode-<version>` from an app
    // token; a mechanical version bump has no design to approve (PR #299).
    ["automation ci branch", { headRepoFullName: REPO, headRef: "ci/bump-opencode-1.18.29" }, true],
    ["fork ci branch", { headRepoFullName: "contributor/aft", headRef: "ci/bump-opencode-9.9.9" }, false],
    // `train/**` is a path prefix, not a substring: a branch simply named
    // `train` or `trainer` is an ordinary branch.
    ["bare train branch name", { headRepoFullName: REPO, headRef: "train" }, false],
    ["trainer branch", { headRepoFullName: REPO, headRef: "trainer" }, false],
    // A fork cannot buy a bypass by naming its branch after a maintainer one.
    ["fork train branch", { headRepoFullName: "contributor/aft", headRef: "train/sneaky" }, false],
    ["fork feature branch", { headRepoFullName: "contributor/aft", headRef: "feature/x" }, false],
    ["maintainer feature branch", { headRepoFullName: REPO, headRef: "feature/x" }, false],
    ["trivial label", { labels: ["trivial"] }, true],
    ["trivial label on a fork", { headRepoFullName: "contributor/aft", labels: ["trivial"] }, true],
    ["unrelated label", { labels: ["bug"] }, false],
  ];

  for (const [name, overrides, skipped] of cases) {
    test(name, () => {
      const reason = pullRequestSkipReason(pullRequest(overrides), REPO);
      assert.equal(reason !== null, skipped, `reason: ${reason}`);
    });
  }
});

describe("decide", () => {
  const approved = { number: 42, state: "open", labels: ["design-approved"] };
  const unapproved = { number: 42, state: "open", labels: ["enhancement"] };

  test("no linked issue fails with the contributor-facing text", () => {
    const decision = decide({
      pullRequest: pullRequest({ body: "Just a fix" }),
      repoFullName: REPO,
    });
    assert.equal(decision.conclusion, "failure");
    assert.equal(decision.message, GATE_MESSAGE);
    assert.equal(decision.linkedIssue, null);
  });

  test("linked issue without the label fails and names the issue", () => {
    const decision = decide({
      pullRequest: pullRequest({ body: "Closes #42" }),
      repoFullName: REPO,
      issue: unapproved,
    });
    assert.equal(decision.conclusion, "failure");
    assert.equal(decision.linkedIssue, 42);
    assert.ok(decision.message.startsWith("Waiting for `design-approved` on #42."));
    assert.ok(decision.message.includes(GATE_MESSAGE));
  });

  test("linked issue that cannot be read fails closed", () => {
    const decision = decide({
      pullRequest: pullRequest({ body: "Closes #999" }),
      repoFullName: REPO,
      issue: null,
    });
    assert.equal(decision.conclusion, "failure");
    assert.equal(decision.message, GATE_MESSAGE);
  });

  // Every accepted link form satisfies the gate against an approved issue,
  // not only the closing keywords the gate used to require.
  for (const body of ["Approved issue: #42", "Refs #42", "Part of #42", "Closes #42"]) {
    test(`${JSON.stringify(body)} on a design-approved issue passes`, () => {
      const decision = decide({ pullRequest: pullRequest({ body }), repoFullName: REPO, issue: approved });
      assert.equal(decision.conclusion, "success");
      assert.equal(decision.linkedIssue, 42);
    });
  }

  test("linked issue with the label passes", () => {
    const decision = decide({
      pullRequest: pullRequest({ body: "Closes #42" }),
      repoFullName: REPO,
      issue: approved,
    });
    assert.equal(decision.conclusion, "success");
    assert.equal(decision.linkedIssue, 42);
    assert.equal(decision.skipped, false);
  });

  test("skipped pull requests pass without looking at any issue", () => {
    const decision = decide({
      pullRequest: pullRequest({ labels: ["trivial"], body: "no link at all" }),
      repoFullName: REPO,
    });
    assert.equal(decision.conclusion, "success");
    assert.equal(decision.skipped, true);
  });

  // Both halves of the rule are pinned here on purpose. `opened` and
  // `ready_for_review` are the moments a pull request enters the ready state
  // and both convert on failure; the remaining actions are moments an
  // already-ready pull request changes and must stay comment-only, so that a
  // reader who sees `opened` converting cannot generalise it to all five.
  const draftCases = [
    ["ready_for_review", "failure-no-issue", "", null, true],
    ["ready_for_review", "failure-unapproved", "Closes #42", unapproved, true],
    ["ready_for_review", "success", "Closes #42", approved, false],
    ["opened", "failure-no-issue", "", null, true],
    ["opened", "failure-unapproved", "Closes #42", unapproved, true],
    ["opened", "success", "Closes #42", approved, false],
    ["synchronize", "failure-unapproved", "Closes #42", unapproved, false],
    ["edited", "failure-no-issue", "", null, false],
    ["reopened", "failure-unapproved", "Closes #42", unapproved, false],
    // Label changes re-run the gate (the caller triggers on them so `trivial`
    // takes effect at once) but are not the pull request entering ready.
    ["labeled", "failure-no-issue", "", null, false],
    ["unlabeled", "failure-unapproved", "Refs #42", unapproved, false],
  ];

  for (const [action, name, body, issue, convertToDraft] of draftCases) {
    test(`${action} / ${name} -> convertToDraft=${convertToDraft}`, () => {
      const decision = decide({
        pullRequest: pullRequest({ body }),
        repoFullName: REPO,
        issue,
        action,
      });
      assert.equal(decision.convertToDraft, convertToDraft);
    });
  }
});

describe("contributor-facing text", () => {
  test("is exactly the agreed wording", () => {
    assert.equal(
      GATE_MESSAGE,
      "This PR needs a linked issue with the `design-approved` label before it can be reviewed " +
        "or merged. Link the approved issue with `Approved issue: #<issue>` (or `Refs #<issue>`) " +
        "in the description. A maintainer will apply the `design-approved` label on the issue, or " +
        "`trivial` on the pull request when there is genuinely no design to agree. The issue " +
        "stays open until the change ships; maintainers take care of it then.",
    );
  });

  // A contributor who is told to write a closing keyword makes GitHub close
  // the issue on merge, before the fix ships. This checks every rendering a
  // blocked contributor can read, against GitHub's closing grammar, so a later
  // edit cannot reintroduce one even as an ordinary word.
  test("the blocked-PR message contains no closing keyword", () => {
    const closingKeyword = /\b(close[sd]?|fix(e[sd])?|resolve[sd]?)\b/i;
    const rendered = [
      decide({ pullRequest: pullRequest({ body: "" }), repoFullName: REPO }),
      decide({
        pullRequest: pullRequest({ body: "Refs #42" }),
        repoFullName: REPO,
        issue: { number: 42, state: "open", labels: [] },
        action: "ready_for_review",
      }),
    ].map((decision) => buildCommentBody(decision, { draftConverted: true }));
    for (const body of [GATE_MESSAGE, ...rendered]) {
      assert.doesNotMatch(body, closingKeyword);
    }
  });

  test("a passing pull request with no existing comment gets no comment", () => {
    const decision = { conclusion: "success", message: "fine", linkedIssue: 42 };
    assert.equal(buildCommentBody(decision, { commentExists: false }), null);
    assert.ok(buildCommentBody(decision, { commentExists: true }).startsWith(COMMENT_MARKER));
  });
});

describe("runPullRequestGate", () => {
  for (const [name, raw] of [
    ["rejects a closed design-approved issue", { state: "closed" }],
    ["rejects a design-approved pull request returned by the issues API", {
      state: "open", pull_request: { url: `https://api.github.com/repos/${REPO}/pulls/42` },
    }],
  ]) {
    test(name, async () => {
      // Drive the REST normalizer too: /issues/N can return a pull request,
      // and a fixture that skips normalization cannot detect lost metadata.
      const reader = createGitHubApi({
        token: "fixture", repoFullName: REPO,
        fetchImpl: async (url) => {
          assert.equal(url, `https://api.github.com/repos/${REPO}/issues/42`);
          return new Response(JSON.stringify({
            number: 42, labels: [{ name: DESIGN_APPROVED_LABEL }], ...raw,
          }));
        },
      });
      const { api, checkApi, state } = createFixtureApi();
      api.getIssue = reader.getIssue;
      const result = await runPullRequestGate({
        api, checkApi, repoFullName: REPO,
        pullRequest: pullRequest({ body: "Refs #42" }),
        action: "synchronize", log: silentLog,
      });
      assert.equal(result.conclusion, "failure");
      assert.equal(result.linkedIssue, 42);
      assert.equal(state.checkRuns[0].conclusion, "failure");
      assert.equal(state.comments.length, 1);
    });
  }

  test("comments once and edits that same comment on later runs", async () => {
    const { api, checkApi, state } = createFixtureApi();
    const pr = pullRequest({ body: "No issue here" });

    const first = await runPullRequestGate({
      api,
      checkApi,
      repoFullName: REPO,
      pullRequest: pr,
      log: silentLog,
    });
    assert.equal(first.conclusion, "failure");
    assert.equal(first.comment.action, "created");
    assert.equal(state.comments.length, 1);
    assert.ok(state.comments[0].body.includes(COMMENT_MARKER));
    assert.ok(state.comments[0].body.includes(GATE_MESSAGE));
    // `opened` converts a failing pull request to draft, so the first comment
    // also carries the draft note.
    assert.ok(state.comments[0].body.includes(draftNote(null)));
    const firstCommentId = state.comments[0].id;

    // `synchronize` does not convert, so the note goes away — but the gate
    // edits the comment it already has rather than opening a second one.
    const second = await runPullRequestGate({
      api,
      checkApi,
      repoFullName: REPO,
      pullRequest: pr,
      action: "synchronize",
      log: silentLog,
    });
    assert.equal(second.comment.action, "updated");
    assert.equal(state.comments.length, 1);
    assert.equal(state.comments[0].id, firstCommentId);
    assert.ok(!state.comments[0].body.includes(draftNote(null)));

    const third = await runPullRequestGate({
      api,
      checkApi,
      repoFullName: REPO,
      pullRequest: pr,
      action: "synchronize",
      log: silentLog,
    });
    assert.equal(third.comment.action, "unchanged");
    assert.equal(state.comments.length, 1);
    assert.equal(state.comments[0].id, firstCommentId);
  });

  test("ready_for_review on a blocked PR converts it back to draft and says so", async () => {
    const pr = pullRequest({ body: "Closes #42" });
    const { api, checkApi, state } = createFixtureApi({
      issues: [{ number: 42, state: "open", labels: [] }],
      pullRequests: [pr],
    });

    const result = await runPullRequestGate({
      api,
      checkApi,
      repoFullName: REPO,
      pullRequest: pr,
      action: "ready_for_review",
      log: silentLog,
    });

    assert.equal(result.conclusion, "failure");
    assert.equal(result.draftConverted, true);
    assert.deepEqual(state.convertedToDraft, [pr.nodeId]);
    assert.ok(
      state.comments[0].body.includes(
        "Converted back to draft; it will be marked ready automatically once #42 is design-approved.",
      ),
    );
  });

  test("a labeled event re-evaluates without converting to draft", async () => {
    // A label event's payload carries the pull request's current labels.
    const blocked = pullRequest({ body: "Refs #42" });
    const { api, checkApi, state } = createFixtureApi({
      issues: [{ number: 42, state: "open", labels: [] }],
      pullRequests: [blocked],
    });
    const failing = await runPullRequestGate({
      api,
      checkApi,
      repoFullName: REPO,
      pullRequest: blocked,
      action: "labeled",
      log: silentLog,
    });
    assert.equal(failing.conclusion, "failure");
    assert.equal(failing.draftConverted, false);
    assert.deepEqual(state.convertedToDraft, []);
    assert.equal(state.checkRuns.at(-1).conclusion, "failure");

    // The maintainer applies `trivial`: the same event re-runs and passes.
    const trivial = { ...blocked, labels: ["trivial"] };
    const passing = await runPullRequestGate({
      api,
      checkApi,
      repoFullName: REPO,
      pullRequest: trivial,
      action: "labeled",
      log: silentLog,
    });
    assert.equal(passing.conclusion, "success");
    assert.deepEqual(state.convertedToDraft, []);
    assert.equal(state.checkRuns.at(-1).conclusion, "success");
  });

  test("a refused draft conversion still fails the gate", async () => {
    const pr = pullRequest({ body: "" });
    const { api, checkApi, state } = createFixtureApi({
      pullRequests: [pr],
      refuse: ["convertPullRequestToDraft", "listComments", "createComment"],
    });

    const result = await runPullRequestGate({
      api,
      checkApi,
      repoFullName: REPO,
      pullRequest: pr,
      action: "ready_for_review",
      log: silentLog,
    });

    assert.equal(result.conclusion, "failure");
    assert.equal(result.draftConverted, false);
    assert.equal(state.comments.length, 0);
    assert.equal(state.convertedToDraft.length, 0);
  });

  // Branch protection requires the check named CHECK_NAME, and
  // `runPullRequestGate` — the path a `pull_request_target` event takes — is
  // what publishes it on an ordinary pull request. Both outcomes are pinned:
  // a publish that hard-coded `success` would still satisfy a test that only
  // exercised a passing pull request, and would wave every blocked one
  // through.
  test("publishes the required check run as a failure when the gate blocks", async () => {
    const pr = pullRequest({ body: "No issue here" });
    const { api, checkApi, state } = createFixtureApi({ pullRequests: [pr] });

    const result = await runPullRequestGate({
      api,
      checkApi,
      repoFullName: REPO,
      pullRequest: pr,
      log: silentLog,
    });

    assert.equal(result.conclusion, "failure");
    assert.equal(state.checkRuns.length, 1);
    assert.equal(state.checkRuns[0].conclusion, "failure");
    assert.equal(state.checkRuns[0].headSha, pr.headSha);
    assert.equal(state.checkRuns[0].summary, GATE_MESSAGE);
  });

  test("publishes the required check run as a success when the gate passes", async () => {
    const pr = pullRequest({ body: "Closes #42" });
    const { api, checkApi, state } = createFixtureApi({
      issues: [{ number: 42, state: "open", labels: ["design-approved"] }],
      pullRequests: [pr],
    });

    const result = await runPullRequestGate({
      api,
      checkApi,
      repoFullName: REPO,
      pullRequest: pr,
      log: silentLog,
    });

    assert.equal(result.conclusion, "success");
    assert.equal(state.checkRuns.length, 1);
    assert.equal(state.checkRuns[0].conclusion, "success");
    assert.equal(state.checkRuns[0].headSha, pr.headSha);
  });

  // Which token publishes the check run is the difference between a gate that
  // blocks on its own and one that blocks only while an App installation
  // happens to carry `checks: write` in this repository. Nothing in a run can
  // read an App's permissions back — that needs the App's own credentials —
  // so a fail-closed path must not rest on one. The required check therefore
  // goes out on the check client, built from the calling job's GITHUB_TOKEN,
  // and the App client is left with the comment and the draft conversion.
  test("publishes the required check run on the check client, not the App client", async () => {
    const pr = pullRequest({ body: "No issue here" });
    const { api, checkApi, state } = createFixtureApi({ pullRequests: [pr] });

    const result = await runPullRequestGate({
      api,
      checkApi,
      repoFullName: REPO,
      pullRequest: pr,
      log: silentLog,
    });

    assert.equal(result.conclusion, "failure");
    assert.deepEqual(
      state.checkRuns.map((run) => run.via),
      ["check"],
      "the design-gate check run must be published with the check token",
    );
  });

  // The name is the contract with branch protection, and it has to come from
  // the script rather than from whatever a calling repository named its job.
  // The fixture never sees the name — the real client supplies it — so pin it
  // where it is actually set, and pin that it is set from CHECK_NAME rather
  // than from a second copy of the string that could drift away from it.
  test("the published check run is named from CHECK_NAME, in one place", () => {
    assert.equal(CHECK_NAME, "design-gate");

    const source = readFileSync(
      new URL("../.github/actions/design-gate/design-gate.mjs", import.meta.url),
      "utf8",
    );
    const checkRunPosts = source.match(/check-runs`,\s*\{\s*\n\s*name:\s*([^,\n]+)/g) ?? [];
    assert.equal(checkRunPosts.length, 1, "expected exactly one check-runs POST");
    assert.ok(
      checkRunPosts[0].includes("name: CHECK_NAME"),
      `the check-runs POST must take its name from CHECK_NAME (found: ${checkRunPosts[0]})`,
    );
  });

  test("the pull-request job is allowed to publish the check run", () => {
    const workflowYaml = readFileSync(
      new URL("../.github/workflows/design-gate.yml", import.meta.url),
      "utf8",
    );
    const prJob = workflowYaml.match(
      /\n  design-gate:\s*\n([\s\S]*?)(?=\n\s{2}[a-zA-Z0-9_-]+:\s*\n|$)/,
    );
    assert.ok(prJob, "design-gate job must be present in workflow");
    assert.ok(
      /checks:\s*write/.test(prJob[1]),
      "the pull-request job needs checks: write, or the required check is never published",
    );
  });

  test("passing gate leaves a clean PR without a comment", async () => {
    const { api, checkApi, state } = createFixtureApi({
      issues: [{ number: 42, state: "open", labels: ["design-approved"] }],
    });
    const result = await runPullRequestGate({
      api,
      checkApi,
      repoFullName: REPO,
      pullRequest: pullRequest({ body: "Closes #42" }),
      log: silentLog,
    });
    assert.equal(result.conclusion, "success");
    assert.equal(state.comments.length, 0);
  });

  test("passing gate closes out an existing blocking comment", async () => {
    const { api, checkApi, state } = createFixtureApi({
      issues: [{ number: 42, state: "open", labels: ["design-approved"] }],
      comments: [{ id: 7, issueNumber: 100, body: `${COMMENT_MARKER}\n\n${GATE_MESSAGE}` }],
    });
    const result = await runPullRequestGate({
      api,
      checkApi,
      repoFullName: REPO,
      pullRequest: pullRequest({ body: "Closes #42" }),
      log: silentLog,
    });
    assert.equal(result.conclusion, "success");
    assert.equal(state.comments.length, 1);
    assert.ok(!state.comments[0].body.includes(GATE_MESSAGE));
    assert.ok(state.comments[0].body.includes("#42 has the `design-approved` label"));
  });

  test("maintainer train branches skip without any API call", async () => {
    const { api, checkApi, state } = createFixtureApi();
    const result = await runPullRequestGate({
      api,
      checkApi,
      repoFullName: REPO,
      pullRequest: pullRequest({ headRepoFullName: REPO, headRef: "train/v0.56", body: "" }),
      log: silentLog,
    });
    assert.equal(result.conclusion, "success");
    assert.equal(result.skipped, true);
    assert.equal(state.comments.length, 0);
  });
});

describe("runIssueLabeled", () => {
  const labelled = { number: 42, state: "open", labels: ["design-approved"] };

  test("marks the waiting draft ready and publishes a green check", async () => {
    const waiting = pullRequest({
      number: 101,
      nodeId: "PR_node_101",
      isDraft: true,
      body: "Closes #42",
      headSha: "b".repeat(40),
    });
    const { api, checkApi, state } = createFixtureApi({
      issues: [labelled],
      pullRequests: [waiting],
      comments: [
        {
          id: 3,
          issueNumber: 101,
          body: `${COMMENT_MARKER}\n\n${CONVERTED_MARKER}\n\n${GATE_MESSAGE}`,
        },
      ],
    });

    const { released } = await runIssueLabeled({
      api,
      checkApi,
      repoFullName: REPO,
      issue: labelled,
      log: silentLog,
    });

    assert.deepEqual(released, [101]);
    assert.deepEqual(state.readyForReview, ["PR_node_101"]);
    assert.equal(state.pullRequests.get(101).isDraft, false);
    assert.equal(state.checkRuns.length, 1);
    assert.equal(state.checkRuns[0].conclusion, "success");
    assert.equal(state.checkRuns[0].headSha, "b".repeat(40));
    assert.ok(!state.comments[0].body.includes(GATE_MESSAGE));
  });

  test("releases a draft linked with the template's non-closing line", async () => {
    const waiting = pullRequest({
      number: 106,
      nodeId: "PR_node_106",
      isDraft: true,
      body: "Approved issue: #42",
    });
    const { api, checkApi, state } = createFixtureApi({
      issues: [labelled],
      pullRequests: [waiting],
      comments: [
        {
          id: 4,
          issueNumber: 106,
          body: `${COMMENT_MARKER}\n\n${CONVERTED_MARKER}\n\n${GATE_MESSAGE}`,
        },
      ],
    });

    const { released } = await runIssueLabeled({
      api,
      checkApi,
      repoFullName: REPO,
      issue: labelled,
      log: silentLog,
    });

    assert.deepEqual(released, [106]);
    assert.equal(state.checkRuns[0].conclusion, "success");
  });

  test("ignores drafts that only mention the issue without a linking keyword", async () => {
    const mention = pullRequest({
      number: 102,
      nodeId: "PR_node_102",
      isDraft: true,
      body: "Follows the discussion in #42 but closes nothing",
    });
    const { api, checkApi, state } = createFixtureApi({ issues: [labelled], pullRequests: [mention] });

    const { released } = await runIssueLabeled({
      api,
      checkApi,
      repoFullName: REPO,
      issue: labelled,
      log: silentLog,
    });

    assert.deepEqual(released, []);
    assert.equal(state.readyForReview.length, 0);
    assert.equal(state.checkRuns.length, 0);
  });

  test("ignores drafts linking a different issue", async () => {
    const other = pullRequest({
      number: 103,
      nodeId: "PR_node_103",
      isDraft: true,
      // The search index matches the literal "#42" in the prose below even
      // though the closing keyword points elsewhere.
      body: "Closes #7, part of the same effort as #42",
    });
    const { api, checkApi, state } = createFixtureApi({ issues: [labelled], pullRequests: [other] });

    const { released } = await runIssueLabeled({
      api,
      checkApi,
      repoFullName: REPO,
      issue: labelled,
      log: silentLog,
    });

    assert.deepEqual(released, []);
    assert.equal(state.readyForReview.length, 0);
  });

  test("re-evaluates open non-draft pull requests and publishes check run", async () => {
    const ready = pullRequest({
      number: 104,
      nodeId: "PR_node_104",
      isDraft: false,
      body: "Closes #42",
      headSha: "c".repeat(40),
    });
    const { api, checkApi, state } = createFixtureApi({
      issues: [labelled],
      pullRequests: [ready],
      comments: [{ id: 5, issueNumber: 104, body: `${COMMENT_MARKER}\n\n${GATE_MESSAGE}` }],
    });

    const { released, updated } = await runIssueLabeled({
      api,
      checkApi,
      repoFullName: REPO,
      issue: labelled,
      log: silentLog,
    });

    assert.deepEqual(released, []);
    assert.deepEqual(updated, [104]);
    assert.equal(state.readyForReview.length, 0);
    assert.equal(state.checkRuns.length, 1);
    assert.equal(state.checkRuns[0].conclusion, "success");
    assert.equal(state.checkRuns[0].headSha, "c".repeat(40));
    assert.ok(!state.comments[0].body.includes(GATE_MESSAGE));
  });

  test("ignores closed pull requests", async () => {
    const closed = pullRequest({
      number: 105,
      nodeId: "PR_node_105",
      state: "closed",
      isDraft: false,
      body: "Closes #42",
    });
    const { api, checkApi, state } = createFixtureApi({ issues: [labelled], pullRequests: [closed] });

    const { released, updated } = await runIssueLabeled({
      api,
      checkApi,
      repoFullName: REPO,
      issue: labelled,
      log: silentLog,
    });

    assert.deepEqual(released, []);
    assert.deepEqual(updated, []);
    assert.equal(state.readyForReview.length, 0);
    assert.equal(state.checkRuns.length, 0);
  });

  // Approval undoes only what the gate did. The following arms drive the pull
  // request arm first, so the record the issue arm reads is the one the gate
  // actually writes rather than a hand-built comment.
  function waitingOnApproval(number) {
    const issue = { number: 42, state: "open", labels: [] };
    const pr = pullRequest({ number, nodeId: `PR_node_${number}`, body: "Refs #42" });
    const fixture = createFixtureApi({ issues: [issue], pullRequests: [pr] });
    const runGate = (action) =>
      runPullRequestGate({ ...fixture, repoFullName: REPO, pullRequest: pr, action, log: silentLog });
    const approve = () => {
      issue.labels = ["design-approved"];
      return runIssueLabeled({ ...fixture, repoFullName: REPO, issue, log: silentLog });
    };
    return { ...fixture, pr, runGate, approve };
  }

  test("releases a draft the gate converted when its issue is approved", async () => {
    const { pr, state, runGate, approve } = waitingOnApproval(110);

    const blocked = await runGate("opened");
    assert.equal(blocked.draftConverted, true);
    assert.equal(pr.isDraft, true);
    assert.ok(state.comments[0].body.includes(CONVERTED_MARKER));

    const { released } = await approve();

    assert.deepEqual(released, [110]);
    assert.deepEqual(state.readyForReview, ["PR_node_110"]);
    assert.equal(pr.isDraft, false);
    assert.equal(state.checkRuns.at(-1).conclusion, "success");
    // Released, so the hold is over and the record goes with it.
    assert.ok(!state.comments[0].body.includes(CONVERTED_MARKER));
  });

  test("does not release an author's own draft linking the approved issue", async () => {
    // One draft has never been touched by the gate; the other has a gate
    // comment (the gate blocked it) but was already a draft, so the gate never
    // converted it. Neither is the gate's to release.
    const noComment = pullRequest({
      number: 111,
      nodeId: "PR_node_111",
      isDraft: true,
      body: "Draft for design guidance. Refs #42",
      headSha: "d".repeat(40),
    });
    const unmarked = pullRequest({
      number: 112,
      nodeId: "PR_node_112",
      isDraft: true,
      body: "Refs #42",
      headSha: "e".repeat(40),
    });
    const { api, checkApi, state } = createFixtureApi({
      issues: [labelled],
      pullRequests: [noComment, unmarked],
      comments: [{ id: 9, issueNumber: 112, body: `${COMMENT_MARKER}\n\n${GATE_MESSAGE}` }],
    });

    const { released, updated } = await runIssueLabeled({
      api,
      checkApi,
      repoFullName: REPO,
      issue: labelled,
      log: silentLog,
    });

    assert.deepEqual(released, []);
    assert.deepEqual(state.readyForReview, []);
    assert.equal(noComment.isDraft, true);
    assert.equal(unmarked.isDraft, true);
    assert.deepEqual(updated, [111, 112]);
    assert.deepEqual(
      state.checkRuns.map((run) => [run.headSha, run.conclusion, run.via]),
      [
        ["d".repeat(40), "success", "check"],
        ["e".repeat(40), "success", "check"],
      ],
    );
    assert.ok(!state.comments[0].body.includes(GATE_MESSAGE));
  });

  test("the converted marker survives a synchronize run while the PR stays a draft", async () => {
    const { pr, state, runGate, approve } = waitingOnApproval(113);

    await runGate("opened");
    assert.equal(pr.isDraft, true);

    const pushed = await runGate("synchronize");
    assert.equal(pushed.draftConverted, false);
    assert.equal(state.comments.length, 1);
    assert.ok(state.comments[0].body.includes(CONVERTED_MARKER));
    assert.ok(state.comments[0].body.includes(draftNote(42)));

    const { released } = await approve();
    assert.deepEqual(released, [113]);
    assert.equal(pr.isDraft, false);
  });

  test("the converted marker is cleared once a run sees the PR out of draft", async () => {
    const { pr, state, runGate, approve } = waitingOnApproval(114);

    await runGate("opened");
    assert.ok(state.comments[0].body.includes(CONVERTED_MARKER));

    // Someone marks it ready; the next run sees it out of draft.
    pr.isDraft = false;
    await runGate("synchronize");
    assert.ok(!state.comments[0].body.includes(CONVERTED_MARKER));

    // The author then turns it back into a draft themselves. That draft is
    // theirs, so approval leaves it alone.
    pr.isDraft = true;
    const { released, updated } = await approve();
    assert.deepEqual(released, []);
    assert.deepEqual(updated, [114]);
    assert.equal(pr.isDraft, true);
    assert.equal(state.checkRuns.at(-1).conclusion, "success");
  });

  test("an unreadable gate comment releases nothing but still publishes the check", async () => {
    const waiting = pullRequest({
      number: 115,
      nodeId: "PR_node_115",
      isDraft: true,
      body: "Refs #42",
      headSha: "f".repeat(40),
    });
    const warnings = [];
    const { api, checkApi, state } = createFixtureApi({
      issues: [labelled],
      pullRequests: [waiting],
      comments: [
        {
          id: 10,
          issueNumber: 115,
          body: `${COMMENT_MARKER}\n\n${CONVERTED_MARKER}\n\n${GATE_MESSAGE}`,
        },
      ],
      refuse: ["listComments"],
    });

    const { released, updated } = await runIssueLabeled({
      api,
      checkApi,
      repoFullName: REPO,
      issue: labelled,
      log: { warn: (message) => warnings.push(message) },
    });

    assert.deepEqual(released, []);
    assert.deepEqual(state.readyForReview, []);
    assert.equal(waiting.isDraft, true);
    assert.deepEqual(updated, [115]);
    assert.equal(state.checkRuns.length, 1);
    assert.equal(state.checkRuns[0].conclusion, "success");
    assert.equal(state.checkRuns[0].headSha, "f".repeat(40));
    assert.equal(warnings.length, 1);
    assert.match(warnings[0], /#115/);
  });
});

/**
 * Repo root, found by walking up from this file until a directory carrying a
 * repository marker appears. Anchoring on a marker rather than on a fixed
 * number of `..` segments means moving this test file cannot quietly redirect
 * the security arms at a path that does not exist, and anchoring on the file
 * rather than on `process.cwd()` means the arms read the same files however
 * the runner was invoked.
 */
const REPO_ROOT = (() => {
  let dir = dirname(fileURLToPath(import.meta.url));
  for (;;) {
    if (existsSync(join(dir, ".git")) || existsSync(join(dir, "Cargo.toml"))) return dir;
    const parent = dirname(dir);
    if (parent === dir) return dirname(dirname(fileURLToPath(import.meta.url)));
    dir = parent;
  }
})();

/**
 * The files that actually run the gate. The security properties have to be
 * asserted against these two and nowhere else: an assertion pointed at a file
 * nobody executes proves nothing about what executes.
 */
const GATE_FILES = [
  { label: "reusable workflow", relative: ".github/workflows/design-gate.yml" },
  { label: "composite action", relative: ".github/actions/design-gate/action.yml" },
].map(({ label, relative }) => {
  const path = join(REPO_ROOT, relative);
  // Read defensively. Reading at describe time and letting ENOENT escape would
  // abort the block before it registered its tests, and `node --test` reports
  // that as a SMALLER suite with one failure — a broken fixture that reads as
  // a suite which mostly passes. The arms below are registered either way and
  // an absent file fails an arm that names the missing path.
  let yaml = null;
  try {
    yaml = readFileSync(path, "utf8");
  } catch {
    yaml = null;
  }
  return { label, path, yaml };
});

function gateFileYaml(file) {
  assert.ok(file.yaml !== null, `workflow file missing at ${file.path}`);
  return file.yaml;
}

/** Drop whole-line YAML comments so prose about a step is not read as a step. */
function stripCommentLines(yaml) {
  return yaml
    .split("\n")
    .filter((line) => !/^\s*#/.test(line))
    .join("\n");
}

/** Every action reference on a `uses:` line. */
function usesReferences(yaml) {
  return stripCommentLines(yaml)
    .split("\n")
    .map((line) => line.match(/^\s*(?:-\s+)?uses:\s*(\S+)/))
    .filter((match) => match !== null)
    .map((match) => match[1]);
}

/** Every `run:` script body, inline or block scalar. */
function runScripts(yaml) {
  const lines = yaml.split("\n");
  const scripts = [];
  for (let index = 0; index < lines.length; index += 1) {
    const header = lines[index].match(/^(\s*)(?:-\s+)?run:\s*(.*)$/);
    if (!header) continue;
    const [, indent, inline] = header;
    if (inline && !/^[|>]/.test(inline)) {
      scripts.push(inline);
      continue;
    }
    const body = [];
    for (let next = index + 1; next < lines.length; next += 1) {
      const line = lines[next];
      if (line.trim() === "") {
        body.push("");
        continue;
      }
      if (line.match(/^\s*/)[0].length <= indent.length) break;
      body.push(line);
    }
    scripts.push(body.join("\n"));
  }
  return scripts;
}

/**
 * `git` in command position. Written as a delimited token rather than a
 * substring so `$GITHUB_ACTION_PATH` — which the gate step legitimately uses —
 * is not mistaken for an invocation.
 */
const GIT_INVOCATION = /(^|[\s;&|(])git([\s;&|)]|$)/;

function invokesGit(script) {
  return script
    .split("\n")
    .map((line) => line.replace(/#.*$/, ""))
    .some((line) => GIT_INVOCATION.test(line));
}

/** Step blocks in a `steps:` list, one string per step. */
function stepBlocks(yaml) {
  const stepBlocks = [];
  const lines = yaml.split("\n");
  let currentBlock = [];
  let inSteps = false;

  for (const line of lines) {
    if (/^\s*steps:\s*$/.test(line)) {
      inSteps = true;
      continue;
    }
    if (inSteps && /^[^\s#]/.test(line)) {
      if (currentBlock.length) stepBlocks.push(currentBlock.join("\n"));
      currentBlock = [];
      inSteps = false;
      continue;
    }
    if (inSteps && /^\s{0,4}[a-zA-Z]/.test(line)) {
      if (currentBlock.length) stepBlocks.push(currentBlock.join("\n"));
      currentBlock = [];
      inSteps = false;
      continue;
    }
    if (inSteps) {
      if (/^\s*-\s+/.test(line)) {
        if (currentBlock.length) stepBlocks.push(currentBlock.join("\n"));
        currentBlock = [line];
      } else if (currentBlock.length) {
        currentBlock.push(line);
      }
    }
  }
  if (currentBlock.length) stepBlocks.push(currentBlock.join("\n"));
  return stepBlocks;
}

function checkoutStepsReferencingHead(yaml) {
  return stepBlocks(stripCommentLines(yaml))
    .filter((step) => step.includes("actions/checkout"))
    .filter((step) => step.includes("github.event.pull_request.head"));
}

describe("workflow security properties", () => {
  // The gate runs on `pull_request_target`, in the base repository's context
  // with access to its secrets. Contributor code from the pull request head
  // must never be checked out or executed. The old shape relied on
  // `actions/checkout` defaulting to the base ref, which is a property of an
  // argument left out; these arms pin the stronger property that there is no
  // checkout and no `git` to give an argument to in the first place.
  for (const file of GATE_FILES) {
    test(`${file.label} checks nothing out`, () => {
      const references = usesReferences(gateFileYaml(file));
      assert.deepEqual(
        references.filter((reference) => reference.includes("actions/checkout")),
        [],
        `${file.path} must not use actions/checkout`,
      );
    });

    test(`${file.label} runs no git command`, () => {
      const offenders = runScripts(gateFileYaml(file)).filter(invokesGit);
      assert.deepEqual(offenders, [], `${file.path} must not invoke git`);
    });

    test(`${file.label} has no checkout step referencing the PR head`, () => {
      assert.deepEqual(
        checkoutStepsReferencingHead(gateFileYaml(file)),
        [],
        `${file.path} must never check out the pull request head`,
      );
    });
  }

  // The three arms above run against files that (correctly) contain no
  // checkout at all, so the head-ref arm would pass over an empty set no
  // matter how the detector behaved. This pins the detector itself, so the
  // arm cannot rot into a test that asserts nothing.
  test("the head-ref detector catches a checkout of the PR head", () => {
    const unsafe = [
      "jobs:",
      "  gate:",
      "    steps:",
      "      - uses: actions/checkout@v5",
      "        with:",
      "          ref: ${{ github.event.pull_request.head.sha }}",
    ].join("\n");
    assert.equal(checkoutStepsReferencingHead(unsafe).length, 1);
    assert.equal(usesReferences(unsafe).filter((r) => r.includes("actions/checkout")).length, 1);

    const safe = unsafe.split("\n").slice(0, 4).join("\n");
    assert.equal(checkoutStepsReferencingHead(safe).length, 0);
  });

  // Likewise for the git detector: the gate step's own command mentions
  // `$GITHUB_ACTION_PATH`, and a substring check would call that an
  // invocation and pass for the wrong reason ever after.
  test("the git detector reads command position, not substrings", () => {
    assert.equal(invokesGit('node "${GITHUB_ACTION_PATH}/design-gate.mjs" pull-request'), false);
    assert.equal(invokesGit("# git clone would be wrong here"), false);
    assert.equal(invokesGit("git clone https://example.invalid/repo"), true);
    assert.equal(invokesGit("cd /tmp && git checkout $REF"), true);
  });

  // This arm used to read "the gate step must not mention secrets.GITHUB_TOKEN
  // at all", which was the right claim while the step took one token. It now
  // takes two, and the whole point of the second one is that it IS
  // secrets.GITHUB_TOKEN, so the claim is made per input instead: the app
  // token still does the writes that need it, and the required check run is
  // published with the calling job's own token so that blocking a pull
  // request depends on nothing outside this workflow.
  test("pull-request job gives the App token to github-token and GITHUB_TOKEN to check-token", () => {
    const workflowYaml = gateFileYaml(GATE_FILES[0]);

    // Isolate the pull-request job (design-gate)
    const prJobMatch = workflowYaml.match(
      /\n  design-gate:\s*\n([\s\S]*?)(?=\n\s{2}[a-zA-Z0-9_-]+:\s*\n|$)/,
    );
    assert.ok(prJobMatch, "design-gate job must be present in workflow");
    const prJobYaml = prJobMatch[1];

    // Find the App token step id
    const appTokenStepMatch =
      prJobYaml.match(/id:\s*([a-zA-Z0-9_-]+)\s*\n\s*uses:\s*actions\/create-github-app-token/m) ||
      prJobYaml.match(
        /uses:\s*actions\/create-github-app-token[^\n]*\n[\s\S]*?id:\s*([a-zA-Z0-9_-]+)/m,
      );
    assert.ok(appTokenStepMatch, "must have an actions/create-github-app-token step with an id");
    const appTokenId = appTokenStepMatch[1];

    // The gate now runs as a composite action rather than an inline `run:`, so
    // the token arrives as the action's `github-token` input.
    const gateStep = stepBlocks(stripCommentLines(prJobYaml)).find((step) =>
      step.includes("actions/design-gate@"),
    );
    assert.ok(gateStep, "must have a step using the design-gate composite action");
    assert.ok(
      gateStep.includes("mode: pull-request"),
      `pull-request job must run the gate in pull-request mode (found: ${gateStep})`,
    );

    const githubTokenValue = gateStep.match(/github-token:\s*(.+)/)?.[1] ?? "";
    const checkTokenValue = gateStep.match(/check-token:\s*(.+)/)?.[1] ?? "";

    assert.ok(
      !githubTokenValue.includes("secrets.GITHUB_TOKEN"),
      `the comment and draft conversion must run on the App token (found: ${githubTokenValue})`,
    );

    const expectedOutput = `steps.${appTokenId}.outputs.token`;
    assert.ok(
      githubTokenValue.includes(expectedOutput),
      `github-token must reference ${expectedOutput} (found: ${githubTokenValue})`,
    );

    assert.ok(
      checkTokenValue.includes("secrets.GITHUB_TOKEN"),
      `the check run must be published with the job's own GITHUB_TOKEN (found: ${checkTokenValue})`,
    );
  });

  // The workflow can hand over the right token and the action can still drop
  // it. This pins the other half of the wiring: the input exists, is required
  // so a caller cannot leave it out, and reaches the script as CHECK_TOKEN.
  test("the composite action requires check-token and passes it to the script", () => {
    const actionYaml = gateFileYaml(GATE_FILES[1]);

    const checkTokenInput = actionYaml.match(
      /\n  check-token:\n([\s\S]*?)(?=\n  [a-zA-Z0-9-]+:\n|\nruns:)/,
    );
    assert.ok(checkTokenInput, "the gate action must declare a check-token input");
    assert.match(
      checkTokenInput[1],
      /required:\s*true/,
      "check-token must be required, or a caller can omit the token the check run needs",
    );

    assert.match(
      actionYaml,
      /CHECK_TOKEN:\s*\$\{\{\s*inputs\.check-token\s*\}\}/,
      "the gate step must receive check-token as CHECK_TOKEN",
    );
    assert.match(
      actionYaml,
      /GITHUB_TOKEN:\s*\$\{\{\s*inputs\.github-token\s*\}\}/,
      "the gate step must still receive github-token as GITHUB_TOKEN",
    );
  });
});
