// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Tests for area-reviewers.js — run with: node --test .github/scripts/area-reviewers.test.js

"use strict";

const { describe, it } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const path = require("path");

const {
  patternToRegExp,
  parseConfig,
  matchGroups,
  pickMember,
  planAssignments,
  parsePreviousPicks,
  buildComment,
  run,
} = require("./area-reviewers.js");

const CONFIG = `
strategy = round-robin

group api-reviewers = @A @B
group storage = @carol @dave
group storage = @erin   # continuation
group empty =

*.proto               api-reviewers
/vm/devices/storage/  storage
/vm/devices/get/underhill_config/ api-reviewers storage
docs/**/*.md          empty
`;

const matches = (pattern, file) => patternToRegExp(pattern).test(file);

describe("patternToRegExp", () => {
  it("unanchored extension glob matches at any depth", () => {
    assert.ok(matches("*.proto", "a.proto"));
    assert.ok(matches("*.proto", "support/x/src/a.proto"));
    assert.ok(!matches("*.proto", "a.proto.rs"));
    assert.ok(!matches("*.proto", "aproto"));
  });

  it("anchored directory matches contents only under root", () => {
    assert.ok(matches("/vm/devices/storage/", "vm/devices/storage/disk/src/lib.rs"));
    assert.ok(matches("/vm/devices/storage", "vm/devices/storage/x.rs"));
    assert.ok(!matches("/vm/devices/storage/", "other/vm/devices/storage/x.rs"));
    assert.ok(!matches("/vm/devices/storage/", "vm/devices/storage_ext/x.rs"));
  });

  it("pattern with interior slash is anchored", () => {
    assert.ok(matches("vm/kvm", "vm/kvm/src/lib.rs"));
    assert.ok(!matches("vm/kvm", "x/vm/kvm/src/lib.rs"));
  });

  it("single star does not cross segments; double star does", () => {
    assert.ok(matches("/vmm_core/virt*/", "vmm_core/virt_kvm/src/lib.rs"));
    assert.ok(!matches("/a/*.rs", "a/b/c.rs"));
    assert.ok(matches("/a/**/*.rs", "a/b/c.rs"));
    assert.ok(matches("/a/**/*.rs", "a/c.rs"));
    assert.ok(matches("**/foo", "x/y/foo"));
  });

  it("escapes regex metacharacters and supports ?", () => {
    assert.ok(matches("a+b(c).txt", "a+b(c).txt"));
    assert.ok(!matches("a.txt", "abtxt"));
    assert.ok(matches("file?.rs", "file1.rs"));
  });
});

describe("parseConfig", () => {
  it("parses groups, rules and strategy", () => {
    const c = parseConfig(CONFIG);
    assert.equal(c.strategy, "round-robin");
    assert.deepEqual(c.groups.get("api-reviewers"), ["A", "B"]);
    assert.deepEqual(c.groups.get("storage"), ["carol", "dave", "erin"]);
    assert.deepEqual(c.groups.get("empty"), []);
    assert.equal(c.rules.length, 4);
  });

  it("defaults to least-loaded", () => {
    assert.equal(parseConfig("group g = @a\n*.x g").strategy, "least-loaded");
  });

  it("rejects undefined groups, bad members and bad strategy", () => {
    assert.throws(() => parseConfig("*.proto nope"), /undefined group 'nope'/);
    assert.throws(() => parseConfig("group g = bob"), /invalid member 'bob'/);
    assert.throws(() => parseConfig("group g = @org/team"), /invalid member/);
    assert.throws(() => parseConfig("strategy = yolo"), /unknown strategy/);
    assert.throws(() => parseConfig("group g = @a\n*.proto"), /has no groups/);
  });

  it("parses the checked-in AREA_REVIEWERS", () => {
    const text = fs.readFileSync(path.join(__dirname, "..", "AREA_REVIEWERS"), "utf8");
    const c = parseConfig(text);
    assert.ok(c.rules.length > 0);
  });
});

describe("matchGroups", () => {
  const c = parseConfig(CONFIG);

  it("unions all matching rules and records reasons", () => {
    const m = matchGroups(c, [
      "vm/devices/get/underhill_config/src/lib.rs",
      "openhcl/diag_proto/src/diag.proto",
      "README.md",
    ]);
    assert.deepEqual(m.map((x) => x.group), ["api-reviewers", "storage"]);
    assert.deepEqual(m[0].patterns.sort(), ["*.proto", "/vm/devices/get/underhill_config/"].sort());
    assert.equal(m[0].files.length, 2);
    assert.deepEqual(m[1].patterns, ["/vm/devices/get/underhill_config/"]);
  });

  it("returns nothing when no rule matches", () => {
    assert.deepEqual(matchGroups(c, ["README.md"]), []);
  });
});

describe("pickMember", () => {
  it("round-robin rotates by PR number", () => {
    assert.equal(pickMember(["a", "b", "c"], "round-robin", { prNumber: 0 }), "a");
    assert.equal(pickMember(["a", "b", "c"], "round-robin", { prNumber: 4 }), "b");
  });

  it("random uses rng", () => {
    assert.equal(pickMember(["a", "b"], "random", { rng: () => 0.99 }), "b");
    assert.equal(pickMember(["a", "b"], "random", { rng: () => 0 }), "a");
  });

  it("least-loaded picks minimum, tie-broken round-robin", () => {
    const loads = new Map([["a", 5], ["b", 1], ["c", 1]]);
    assert.equal(pickMember(["a", "b", "c"], "least-loaded", { prNumber: 0, loads }), "b");
    assert.equal(pickMember(["a", "b", "c"], "least-loaded", { prNumber: 1, loads }), "c");
  });

  it("least-loaded falls back to round-robin without load data", () => {
    assert.equal(pickMember(["a", "b"], "least-loaded", { prNumber: 1, loads: new Map() }), "b");
  });

  it("returns null with no candidates", () => {
    assert.equal(pickMember([], "random", {}), null);
  });
});

describe("planAssignments", () => {
  const c = parseConfig(CONFIG);

  it("selects one member per matching group", async () => {
    const plan = await planAssignments(c, { files: ["x.proto"], author: "zed", prNumber: 1 });
    assert.equal(plan.picks.length, 1);
    assert.equal(plan.picks[0].group, "api-reviewers");
    assert.equal(plan.picks[0].user, "B");
  });

  it("never selects the PR author", async () => {
    const plan = await planAssignments(c, { files: ["x.proto"], author: "b", prNumber: 1 });
    assert.equal(plan.picks[0].user, "A");
  });

  it("reports a group whose only eligible member is the author", async () => {
    const solo = parseConfig("group g = @a\n*.x g");
    const plan = await planAssignments(solo, { files: ["f.x"], author: "a", prNumber: 0 });
    assert.equal(plan.picks.length, 0);
    assert.match(plan.unstaffed[0].reason, /author/);
  });

  it("reports empty groups as unstaffed", async () => {
    const plan = await planAssignments(c, { files: ["docs/a/b.md"], author: "z", prNumber: 0 });
    assert.deepEqual(plan.unstaffed.map((u) => u.group), ["empty"]);
  });

  it("treats a group as covered when a member is already involved", async () => {
    const plan = await planAssignments(c, {
      files: ["x.proto"],
      author: "z",
      prNumber: 1,
      involved: ["a"],
    });
    assert.equal(plan.picks.length, 0);
    assert.equal(plan.covered[0].by, "A");
  });

  it("does not re-roll previously recorded picks", async () => {
    const plan = await planAssignments(c, {
      files: ["x.proto"],
      author: "z",
      prNumber: 1,
      previousPicks: { "api-reviewers": "A" },
    });
    assert.equal(plan.picks.length, 0);
    assert.equal(plan.covered[0].why, "previously selected");
  });

  it("a person picked for one group can cover another group they belong to", async () => {
    const overlap = parseConfig("strategy = round-robin\ngroup a = @x @y\ngroup b = @y @z\n*.a a\n*.b b");
    const plan = await planAssignments(overlap, { files: ["f.a", "f.b"], author: "q", prNumber: 1 });
    assert.deepEqual(plan.picks.map((p) => p.user), ["y"]);
    assert.equal(plan.covered[0].group, "b");
  });

  it("least-loaded uses getLoad and accounts for picks made in the same run", async () => {
    const ll = parseConfig("group a = @x @y\n*.a a");
    const calls = [];
    const plan = await planAssignments(ll, {
      files: ["f.a"],
      author: "q",
      prNumber: 0,
      getLoad: async (u) => (calls.push(u), u === "x" ? 3 : 0),
    });
    assert.equal(plan.picks[0].user, "y");
    assert.deepEqual(calls, ["x", "y"]);
  });
});

describe("parsePreviousPicks / buildComment", () => {
  it("round-trips selection state through the comment", () => {
    const picks = [
      { group: "api-reviewers", user: "B", patterns: ["*.proto"], files: ["a/x.proto"] },
    ];
    const body = buildComment(picks, [], "random", "https://example/AREA_REVIEWERS");
    assert.match(body, /@B was selected from `api-reviewers` due to `\*\.proto` changes \(e\.g\. `a\/x\.proto`\)/);
    assert.deepEqual(parsePreviousPicks([body]), { "api-reviewers": "B" });
  });

  it("merges state from multiple comments and ignores garbage", () => {
    const merged = parsePreviousPicks([
      '<!-- area-reviewers {"a":"x"} -->',
      "<!-- area-reviewers {not json} -->",
      '<!-- area-reviewers {"b":"y"} -->',
      "unrelated",
    ]);
    assert.deepEqual(merged, { a: "x", b: "y" });
  });

  it("mentions unstaffed groups", () => {
    const body = buildComment([], [{ group: "g", reason: "no members configured" }], "random", "u");
    assert.match(body, /No reviewer selected from `g`: no members configured/);
  });
});

describe("run (mocked GitHub API)", () => {
  function mockGithub({ files, draft = false, comments = [], requested = [], author = "author" }) {
    const calls = { requestReviewers: [], addAssignees: [], createComment: [] };
    const listFiles = Symbol("listFiles");
    const listReviews = Symbol("listReviews");
    const listComments = Symbol("listComments");
    const github = {
      paginate: async (fn) => {
        if (fn === listFiles) return files.map((filename) => ({ filename }));
        if (fn === listReviews) return [];
        if (fn === listComments) return comments;
        throw new Error("unexpected paginate");
      },
      rest: {
        pulls: {
          get: async () => ({
            data: {
              state: "open",
              draft,
              user: { login: author },
              base: { ref: "main" },
              requested_reviewers: requested.map((login) => ({ login })),
              assignees: [],
            },
          }),
          listFiles,
          listReviews,
          requestReviewers: async (a) => calls.requestReviewers.push(a.reviewers),
        },
        issues: {
          listComments,
          addAssignees: async (a) => calls.addAssignees.push(a.assignees),
          createComment: async (a) => calls.createComment.push(a.body),
        },
        search: {
          issuesAndPullRequests: async () => ({ data: { total_count: 0 } }),
        },
      },
    };
    return { github, calls };
  }

  const context = { repo: { owner: "o", repo: "r" }, payload: { pull_request: { number: 7 } } };
  const core = { warning: () => {} };

  it("runs end-to-end against the checked-in config without throwing", async () => {
    const { github, calls } = mockGithub({ files: ["README.md"] });
    await run(github, context, core);
    assert.equal(calls.createComment.length, 0);
  });

  it("skips drafts", async () => {
    const { github, calls } = mockGithub({ files: ["a.proto"], draft: true });
    await run(github, context, core, { configText: CONFIG });
    assert.equal(calls.requestReviewers.length, 0);
    assert.equal(calls.createComment.length, 0);
  });

  it("requests review, assigns and comments for a matching PR", async () => {
    const { github, calls } = mockGithub({ files: ["svc/api.proto", "vm/devices/storage/x.rs"] });
    await run(github, context, core, { configText: CONFIG });
    // PR #7, round-robin: api-reviewers [A,B] -> B; storage [carol,dave,erin] -> dave.
    assert.deepEqual(calls.requestReviewers, [["B"], ["dave"]]);
    assert.deepEqual(calls.addAssignees, [["B", "dave"]]);
    assert.equal(calls.createComment.length, 1);
    assert.match(calls.createComment[0], /@B was selected from `api-reviewers` due to `\*\.proto` changes/);
    assert.match(calls.createComment[0], /@dave was selected from `storage`/);
    assert.match(calls.createComment[0], /https:\/\/github\.com\/o\/r\/blob\/main\/\.github\/AREA_REVIEWERS/);
  });

  it("is idempotent across pushes", async () => {
    const first = mockGithub({ files: ["svc/api.proto"] });
    await run(first.github, context, core, { configText: CONFIG });
    const prior = [{ user: { type: "Bot" }, body: first.calls.createComment[0] }];
    const second = mockGithub({ files: ["svc/api.proto", "b.proto"], comments: prior });
    await run(second.github, context, core, { configText: CONFIG });
    assert.equal(second.calls.requestReviewers.length, 0);
    assert.equal(second.calls.createComment.length, 0);
  });

  it("does not select when a group member is already a requested reviewer", async () => {
    const { github, calls } = mockGithub({ files: ["a.proto"], requested: ["A"] });
    await run(github, context, core, { configText: CONFIG });
    assert.equal(calls.createComment.length, 0);
  });
});
