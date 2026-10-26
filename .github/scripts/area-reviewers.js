// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Area reviewer assignment — maps the files changed by a PR to reviewer
// groups (configured in .github/AREA_REVIEWERS) and selects ONE concrete
// individual from each matching group. The selected person is requested as a
// reviewer, added as an assignee, and a comment explains why they were picked.
//
// Can be run in two modes:
//   1. GitHub Actions (via actions/github-script): pass github, context, core
//   2. Local preview: `node area-reviewers.js [--base <rev>] [file ...]`
//      Shows which groups the given files (or the diff against the merge-base
//      with <rev>, default origin/main) would match.

"use strict";

const MARKER = "<!-- area-reviewers";
const STRATEGIES = ["least-loaded", "round-robin", "random"];

// --- Pure functions (no GitHub API, easily testable) ---

/**
 * Convert a CODEOWNERS/gitignore-style pattern into a RegExp that matches
 * repository-relative paths (no leading slash).
 */
function patternToRegExp(pattern) {
  let p = pattern;
  let anchored = false;
  if (p.startsWith("/")) {
    anchored = true;
    p = p.slice(1);
  }
  p = p.replace(/\/+$/, "");
  if (p.includes("/")) anchored = true;
  if (p === "" || p === "**") return /^.*$/;

  let body = "";
  for (let i = 0; i < p.length; i++) {
    const c = p[i];
    if (c === "*") {
      if (p[i + 1] === "*") {
        const atSegStart = i === 0 || p[i - 1] === "/";
        if (atSegStart && p[i + 2] === "/") {
          body += "(?:.*/)?";
          i += 2;
        } else {
          body += ".*";
          i += 1;
        }
      } else {
        body += "[^/]*";
      }
    } else if (c === "?") {
      body += "[^/]";
    } else {
      body += c.replace(/[.+^${}()|[\]\\]/g, "\\$&");
    }
  }

  // A pattern that names a directory also matches everything beneath it.
  return new RegExp(`^${anchored ? "" : "(?:.*/)?"}${body}(?:/.*)?$`);
}

/** Normalize a GitHub login: strip leading '@', lowercase for comparison. */
function normLogin(login) {
  return login.replace(/^@/, "").toLowerCase();
}

/**
 * Parse the AREA_REVIEWERS config.
 * Returns { strategy, groups: Map<name, string[]>, rules: [{pattern, regex, groups, line}] }.
 * Throws on syntax errors so misconfiguration is loud.
 */
function parseConfig(text) {
  const groups = new Map();
  const rules = [];
  let strategy = "least-loaded";
  const errors = [];

  text.split(/\r?\n/).forEach((raw, idx) => {
    const lineNo = idx + 1;
    const line = raw.replace(/(^|\s)#.*$/, "").trim();
    if (!line) return;

    let m = line.match(/^group\s+([A-Za-z0-9_.-]+)\s*=\s*(.*)$/);
    if (m) {
      const [, name, rest] = m;
      const members = groups.get(name) || [];
      for (const tok of rest.split(/\s+/).filter(Boolean)) {
        if (!/^@[A-Za-z0-9-]+$/.test(tok)) {
          errors.push(`line ${lineNo}: invalid member '${tok}' (expected @username)`);
          continue;
        }
        const login = tok.slice(1);
        if (!members.some((x) => normLogin(x) === normLogin(login))) {
          members.push(login);
        }
      }
      groups.set(name, members);
      return;
    }

    m = line.match(/^strategy\s*=\s*(\S+)$/);
    if (m) {
      if (!STRATEGIES.includes(m[1])) {
        errors.push(`line ${lineNo}: unknown strategy '${m[1]}' (expected ${STRATEGIES.join(", ")})`);
      } else {
        strategy = m[1];
      }
      return;
    }

    const toks = line.split(/\s+/);
    if (toks.length < 2) {
      errors.push(`line ${lineNo}: rule '${line}' has no groups`);
      return;
    }
    const [pattern, ...ruleGroups] = toks;
    rules.push({ pattern, regex: patternToRegExp(pattern), groups: ruleGroups, line: lineNo });
  });

  for (const r of rules) {
    for (const g of r.groups) {
      if (!groups.has(g)) errors.push(`line ${r.line}: rule '${r.pattern}' references undefined group '${g}'`);
    }
  }

  if (errors.length > 0) {
    throw new Error(`Invalid AREA_REVIEWERS config:\n  ${errors.join("\n  ")}`);
  }
  return { strategy, groups, rules };
}

/**
 * Determine which groups match the changed files.
 * Returns an array (in group-definition order) of
 * { group, members, patterns: string[], files: string[] }.
 */
function matchGroups(config, files) {
  const hits = new Map();
  for (const file of files) {
    for (const rule of config.rules) {
      if (!rule.regex.test(file)) continue;
      for (const g of rule.groups) {
        if (!hits.has(g)) hits.set(g, { patterns: new Set(), files: new Set() });
        hits.get(g).patterns.add(rule.pattern);
        hits.get(g).files.add(file);
      }
    }
  }
  const result = [];
  for (const [group, members] of config.groups) {
    const h = hits.get(group);
    if (!h) continue;
    result.push({ group, members, patterns: [...h.patterns], files: [...h.files] });
  }
  return result;
}

/**
 * Pick one member from `candidates` (already filtered for eligibility).
 * `loads` is an optional Map<lowercase login, number> used by least-loaded.
 */
function pickMember(candidates, strategy, { prNumber = 0, loads, rng = Math.random }) {
  if (candidates.length === 0) return null;
  const rr = (list) => list[prNumber % list.length];
  if (strategy === "random") {
    return candidates[Math.floor(rng() * candidates.length) % candidates.length];
  }
  if (strategy === "least-loaded" && loads) {
    const known = candidates.filter((c) => loads.has(normLogin(c)));
    if (known.length > 0) {
      const min = Math.min(...known.map((c) => loads.get(normLogin(c))));
      return rr(known.filter((c) => loads.get(normLogin(c)) === min));
    }
  }
  return rr(candidates);
}

/**
 * Plan reviewer selection for a PR.
 *
 * @param {object} config - parsed config
 * @param {object} opts
 * @param {string[]} opts.files - changed file paths
 * @param {string} opts.author - PR author login
 * @param {number} opts.prNumber
 * @param {string[]} opts.involved - logins already requested/assigned/reviewing
 * @param {object} opts.previousPicks - { group: login } recorded by earlier runs
 * @param {(login: string) => Promise<number|null>} [opts.getLoad] - for least-loaded
 * @param {() => number} [opts.rng]
 * @returns {Promise<{picks, covered, unstaffed}>}
 */
async function planAssignments(config, opts) {
  const { files, author, prNumber, involved = [], previousPicks = {}, getLoad, rng } = opts;
  const authorKey = normLogin(author || "");
  const involvedSet = new Set(involved.map(normLogin));
  const loads = new Map();
  const picks = [];
  const covered = [];
  const unstaffed = [];

  for (const match of matchGroups(config, files)) {
    const { group, members } = match;
    if (members.length === 0) {
      unstaffed.push({ group, reason: "no members configured" });
      continue;
    }

    if (previousPicks[group]) {
      covered.push({ group, by: previousPicks[group], why: "previously selected" });
      continue;
    }
    const existing = members.find((m) => involvedSet.has(normLogin(m)) && normLogin(m) !== authorKey);
    if (existing) {
      covered.push({ group, by: existing, why: "already involved in the PR" });
      continue;
    }

    const candidates = members.filter((m) => normLogin(m) !== authorKey);
    if (candidates.length === 0) {
      unstaffed.push({ group, reason: "the PR author is the only member" });
      continue;
    }

    if (config.strategy === "least-loaded" && getLoad) {
      for (const c of candidates) {
        const key = normLogin(c);
        if (loads.has(key)) continue;
        const n = await getLoad(c);
        if (typeof n === "number") loads.set(key, n);
      }
    }

    const user = pickMember(candidates, config.strategy, { prNumber, loads, rng });
    picks.push({ ...match, user });
    involvedSet.add(normLogin(user));
    if (loads.has(normLogin(user))) loads.set(normLogin(user), loads.get(normLogin(user)) + 1);
  }

  return { picks, covered, unstaffed };
}

/** Extract { group: login } picks recorded in previous bot comments. */
function parsePreviousPicks(commentBodies) {
  const picks = {};
  const re = /<!-- area-reviewers (\{.*?\}) -->/g;
  for (const body of commentBodies) {
    for (const m of body.matchAll(re)) {
      try {
        Object.assign(picks, JSON.parse(m[1]));
      } catch {
        // Ignore a corrupted marker rather than failing the whole run.
      }
    }
  }
  return picks;
}

function fmtList(items, max = 3) {
  const shown = items.slice(0, max).map((x) => `\`${x}\``);
  const extra = items.length > max ? ` and ${items.length - max} more` : "";
  if (shown.length <= 1) return shown.join("") + extra;
  if (extra) return shown.join(", ") + extra;
  return `${shown.slice(0, -1).join(", ")} and ${shown[shown.length - 1]}`;
}

/** Build the PR comment for a set of new picks. */
function buildComment(picks, unstaffed, strategy, configUrl) {
  const state = Object.fromEntries(picks.map((p) => [p.group, p.user]));
  let body = `${MARKER} ${JSON.stringify(state)} -->\n`;
  body += "### Area reviewer assignment\n\n";
  for (const p of picks) {
    body +=
      `- @${p.user} was selected from \`${p.group}\` due to ${fmtList(p.patterns)} changes ` +
      `(e.g. ${fmtList(p.files, 2)}).\n`;
  }
  for (const u of unstaffed) {
    body += `- ⚠️ No reviewer selected from \`${u.group}\`: ${u.reason}.\n`;
  }
  body +=
    `\n<sub>Selected via \`${strategy}\` from [.github/AREA_REVIEWERS](${configUrl}). ` +
    "The selected reviewer is responsible for reviewing (or explicitly handing off) this area. " +
    "To re-roll, remove the reviewer/assignee and delete this comment, then push or re-run the workflow.</sub>\n";
  return body;
}

// --- GitHub Actions entrypoint ---

/**
 * @param {object} github - Octokit REST client
 * @param {object} context - GitHub Actions context
 * @param {object} core - GitHub Actions core
 * @param {object} [opts]
 * @param {string} [opts.configText] - override config contents (for tests)
 */
async function run(github, context, core, opts = {}) {
  const fs = require("fs");
  const path = require("path");
  const { owner, repo } = context.repo;
  const prNumber = context.payload.pull_request.number;

  const configText =
    opts.configText ?? fs.readFileSync(path.join(__dirname, "..", "AREA_REVIEWERS"), "utf8");
  const config = parseConfig(configText);

  // Re-fetch the PR so draft state / reviewers / assignees are current.
  const { data: pr } = await github.rest.pulls.get({ owner, repo, pull_number: prNumber });
  if (pr.state !== "open") {
    console.log("PR is not open — skipping.");
    return;
  }
  if (pr.draft) {
    console.log("PR is a draft — skipping until it is marked ready for review.");
    return;
  }

  const changed = await github.paginate(github.rest.pulls.listFiles, {
    owner,
    repo,
    pull_number: prNumber,
    per_page: 100,
  });
  const files = [...new Set(changed.flatMap((f) => [f.filename, f.previous_filename].filter(Boolean)))];

  const reviews = await github.paginate(github.rest.pulls.listReviews, {
    owner,
    repo,
    pull_number: prNumber,
    per_page: 100,
  });
  const involved = [
    ...(pr.requested_reviewers || []).map((u) => u.login),
    ...(pr.assignees || []).map((u) => u.login),
    ...reviews.map((r) => r.user && r.user.login).filter(Boolean),
  ];

  const comments = await github.paginate(github.rest.issues.listComments, {
    owner,
    repo,
    issue_number: prNumber,
    per_page: 100,
  });
  const botComments = comments.filter(
    (c) => c.user && c.user.type === "Bot" && c.body && c.body.startsWith(MARKER)
  );
  const previousPicks = parsePreviousPicks(botComments.map((c) => c.body));

  async function getLoad(login) {
    try {
      const { data } = await github.rest.search.issuesAndPullRequests({
        q: `repo:${owner}/${repo} is:pr is:open draft:false review-requested:${login}`,
        per_page: 1,
      });
      return data.total_count;
    } catch (e) {
      core.warning(`Could not compute review load for @${login} (${e.status || "?"}): ${e.message}`);
      return null;
    }
  }

  const plan = await planAssignments(config, {
    files,
    author: pr.user.login,
    prNumber,
    involved,
    previousPicks,
    getLoad,
  });

  for (const c of plan.covered) {
    console.log(`Group '${c.group}' already covered by @${c.by} (${c.why}).`);
  }
  for (const u of plan.unstaffed) {
    core.warning(`No reviewer selected from '${u.group}': ${u.reason}.`);
  }
  if (plan.picks.length === 0) {
    console.log("No new reviewer selections needed.");
    return;
  }

  const users = [...new Set(plan.picks.map((p) => p.user))];
  for (const user of users) {
    try {
      await github.rest.pulls.requestReviewers({ owner, repo, pull_number: prNumber, reviewers: [user] });
    } catch (e) {
      core.warning(`Failed to request review from @${user} (${e.status || "?"}): ${e.message}`);
    }
  }
  try {
    await github.rest.issues.addAssignees({ owner, repo, issue_number: prNumber, assignees: users });
  } catch (e) {
    core.warning(`Failed to add assignees (${e.status || "?"}): ${e.message}`);
  }

  // Only report unstaffed groups once, alongside the first real selection.
  const unstaffedToReport = botComments.length === 0 ? plan.unstaffed : [];
  const configUrl = `${context.serverUrl || "https://github.com"}/${owner}/${repo}/blob/${pr.base.ref}/.github/AREA_REVIEWERS`;
  const body = buildComment(plan.picks, unstaffedToReport, config.strategy, configUrl);
  console.log(body);
  await github.rest.issues.createComment({ owner, repo, issue_number: prNumber, body });
}

// --- Local CLI entrypoint ---

function localMain() {
  const fs = require("fs");
  const path = require("path");
  const { execFileSync } = require("child_process");
  const args = process.argv.slice(2);

  if (args.includes("--help") || args.includes("-h")) {
    console.log(
      "Usage:\n" +
        "  node area-reviewers.js                 Preview groups for the diff vs merge-base with origin/main\n" +
        "  node area-reviewers.js --base <rev>    Preview groups for the diff vs merge-base with <rev>\n" +
        "  node area-reviewers.js <file> ...      Preview groups for the given repo-relative paths"
    );
    return;
  }

  const config = parseConfig(fs.readFileSync(path.join(__dirname, "..", "AREA_REVIEWERS"), "utf8"));

  let files;
  const baseIdx = args.indexOf("--base");
  if (args.length > 0 && baseIdx === -1) {
    files = args.map((f) => f.replace(/\\/g, "/").replace(/^\.?\//, ""));
  } else {
    const base = baseIdx >= 0 ? args[baseIdx + 1] : "origin/main";
    const mergeBase = execFileSync("git", ["merge-base", base, "HEAD"], { encoding: "utf8" }).trim();
    files = execFileSync("git", ["diff", "--name-only", mergeBase], { encoding: "utf8" })
      .split(/\r?\n/)
      .filter(Boolean);
  }

  const matches = matchGroups(config, files);
  if (matches.length === 0) {
    console.log(`No groups match ${files.length} changed file(s).`);
    return;
  }
  for (const m of matches) {
    const members = m.members.length ? m.members.map((x) => `@${x}`).join(" ") : "(no members)";
    console.log(`${m.group} [${members}] due to ${m.patterns.join(", ")}`);
    for (const f of m.files.slice(0, 5)) console.log(`    ${f}`);
    if (m.files.length > 5) console.log(`    ... and ${m.files.length - 5} more`);
  }
}

if (require.main === module) {
  try {
    localMain();
  } catch (e) {
    console.error(e.message);
    process.exit(1);
  }
}

module.exports = {
  patternToRegExp,
  parseConfig,
  matchGroups,
  pickMember,
  planAssignments,
  parsePreviousPicks,
  buildComment,
  run,
};
