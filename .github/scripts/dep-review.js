// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Dependency review script — detects external (3rd-party) crate changes in
// Cargo.lock and dependency containment policy violations. When issues are
// found, requests review from the dependency reviewer team. When issues are
// resolved, removes the review request.
//
// Can be run in two modes:
//   1. GitHub Actions (via actions/github-script): pass github, context, core
//   2. Local testing: `node dep-review.js --base <file> --pr <file> [--manifest <root-Cargo.toml> --policy <dep-policy.json>]`

"use strict";

// --- Pure functions (no GitHub API, easily testable) ---

/**
 * Parse a Cargo.lock file into a Set of "name\tversion\tsource" strings
 * for external packages (those with a `source` field).
 *
 * Cargo.lock can contain multiple versions of the same crate from the
 * same registry (e.g., windows-sys 0.48 and 0.52), so each unique
 * (name, version, source) tuple is tracked independently.
 */
function parseExternalDeps(content) {
  const deps = new Set();
  const blocks = content.split(/\n(?=\[\[package\]\])/);
  for (const block of blocks) {
    if (!block.includes("[[package]]")) continue;
    const name = block.match(/^name\s*=\s*"(.+?)"/m)?.[1];
    const version = block.match(/^version\s*=\s*"(.+?)"/m)?.[1];
    const source = block.match(/^source\s*=\s*"(.+?)"/m)?.[1];
    if (!name || !version) continue;
    if (source) {
      deps.add(`${name}\t${version}\t${source}`);
    }
  }
  return deps;
}

/** Format a source string for human-readable display. */
function fmtSource(source) {
  if (source.startsWith("registry+")) return "";
  if (source.startsWith("git+")) {
    const url = source.replace(/^git\+/, "").replace(/#.*$/, "");
    return ` (${url})`;
  }
  return ` (${source})`;
}

/**
 * Diff two parsed dependency sets.
 * Returns { added } array — entries present in prDeps but not baseDeps.
 * Removals are not tracked because dropping a dependency doesn't require review.
 */
function diffDeps(baseDeps, prDeps) {
  const added = [];

  for (const key of prDeps) {
    if (!baseDeps.has(key)) {
      const [name, version, source] = key.split("\t");
      added.push({ name, version, source });
    }
  }

  return { added };
}

/** Build a markdown summary of dependency changes. */
function buildSummary(diff) {
  const { added } = diff;
  let summary = "### External dependency changes detected\n\n";

  if (added.length > 0) {
    summary += "**New external crate versions:**\n";
    for (const d of added) {
      summary += `- \`${d.name}\` ${d.version}${fmtSource(d.source)}\n`;
    }
    summary += "\n";
  }

  return summary;
}

// --- Dependency graph policy checks (reason 2a/2b) ---

/**
 * Parse root Cargo.toml [workspace.dependencies] to build a map of
 * crate name → directory path (only for path-based / internal deps).
 *
 * NOTE: This only covers crates listed in [workspace.dependencies], not all
 * workspace members. Crates that are workspace members but not listed there
 * (e.g., fuzz targets) won't be covered by containment checks. In practice
 * this is fine — such crates are leaf crates unlikely to introduce
 * cross-boundary deps, and resolving all members would require fetching
 * individual Cargo.toml files for each member.
 */
function parseCratePathMap(cargoTomlContent) {
  const map = new Map();
  // Match inline table entries with a path key:
  //   crate_name = { path = "some/dir", ... }
  // Handles optional extra keys before/after `path`.
  const regex = /^([\w][\w-]*)\s*=\s*\{[^}]*path\s*=\s*"([^"]+)"[^}]*\}/gm;
  let match;
  while ((match = regex.exec(cargoTomlContent)) !== null) {
    map.set(match[1], match[2]);
  }
  return map;
}

/**
 * Parse Cargo.lock to extract the internal dependency graph.
 * Returns { graph: Map<name, string[]>, internalCrates: Set<name> }.
 * Only internal crates (those without a `source` field) are included.
 */
function parseInternalDepGraph(lockContent) {
  const graph = new Map();
  const internalCrates = new Set();

  const blocks = lockContent.split(/\n(?=\[\[package\]\])/);
  for (const block of blocks) {
    if (!block.includes("[[package]]")) continue;
    const name = block.match(/^name\s*=\s*"(.+?)"/m)?.[1];
    const source = block.match(/^source\s*=\s*"(.+?)"/m)?.[1];
    if (!name) continue;
    if (source) continue; // external crate

    internalCrates.add(name);

    const depsMatch = block.match(/^dependencies\s*=\s*\[([\s\S]*?)\]/m);
    if (depsMatch) {
      const deps = [];
      for (const entry of depsMatch[1].matchAll(/"([^"]+)"/g)) {
        // Entries are "name" or "name version"
        deps.push(entry[1].split(" ")[0]);
      }
      graph.set(name, deps);
    } else {
      graph.set(name, []);
    }
  }

  return { graph, internalCrates };
}

/**
 * Check containment policies against the internal dep graph.
 * A containment rule says: crates under `prefix` may only have internal
 * dependencies on other crates under `prefix`.
 *
 * Returns an array of violation objects.
 */
function checkContainment(graph, internalCrates, pathMap, policy) {
  const violations = [];

  for (const rule of policy.containment || []) {
    const prefix = rule.prefix;

    for (const [name, path] of pathMap) {
      if (!path.startsWith(prefix)) continue;
      if (!graph.has(name)) continue;

      for (const dep of graph.get(name)) {
        // Only check edges to other internal crates
        if (!internalCrates.has(dep)) continue;
        const depPath = pathMap.get(dep);
        if (!depPath) continue; // can't resolve — skip
        if (depPath.startsWith(prefix)) continue; // same prefix — ok

        violations.push({
          crate: name,
          cratePath: path,
          dep,
          depPath,
          rule: rule.description || `${prefix} containment`,
        });
      }
    }
  }

  return violations;
}

/**
 * Diff containment violations between base and PR.
 * Only returns *new* violations (present in PR but not in base),
 * so existing violations are grandfathered.
 */
function diffContainmentViolations(baseViolations, prViolations) {
  const baseSet = new Set(
    baseViolations.map((v) => `${v.crate}\t${v.dep}`)
  );
  return prViolations.filter((v) => !baseSet.has(`${v.crate}\t${v.dep}`));
}

/** Build a markdown summary of containment violations. */
function buildPolicySummary(violations) {
  if (violations.length === 0) return "";

  let summary = "### Dependency containment violations\n\n";
  summary +=
    "The following new internal dependency edges violate containment policy:\n\n";
  for (const v of violations) {
    summary +=
      `- \`${v.crate}\` (${v.cratePath}) → \`${v.dep}\` (${v.depPath}) — ${v.rule}\n`;
  }
  summary += "\n";
  return summary;
}

// --- GitHub Actions entrypoint ---

const DEP_REVIEW_TEAM = "openvmm-dependency-reviewers";

/**
 * Main function called from actions/github-script.
 * Requests or removes review from the dependency reviewer team based on
 * whether external dep changes or policy violations are detected.
 *
 * @param {object} github - Octokit REST client
 * @param {object} context - GitHub Actions context
 * @param {object} core - GitHub Actions core (for setFailed)
 */
async function run(github, context, core) {
  const prNumber = context.payload.pull_request.number;
  const baseSha = context.payload.pull_request.base.sha;

  // Step 1: Check if Cargo.lock was modified
  let allFiles = [];
  let page = 1;
  const MAX_PAGES = 30;
  while (page <= MAX_PAGES) {
    const { data: files } = await github.rest.pulls.listFiles({
      owner: context.repo.owner,
      repo: context.repo.repo,
      pull_number: prNumber,
      per_page: 100,
      page,
    });
    if (files.length === 0) break;
    allFiles = allFiles.concat(files);
    if (files.length < 100) break;
    page++;
  }

  if (page > MAX_PAGES) {
    core.warning(
      `PR has more than ${MAX_PAGES * 100} changed files — ` +
        `Cargo.lock detection may be incomplete. Assuming it changed.`
    );
  }

  const lockfileChanged = page > MAX_PAGES || allFiles.some((f) => f.filename === "Cargo.lock");
  if (!lockfileChanged) {
    console.log("Cargo.lock not modified — nothing to review.");

    // Remove any stale review request from a previous push that did touch Cargo.lock
    try {
      await github.rest.pulls.removeRequestedReviewers({
        owner: context.repo.owner,
        repo: context.repo.repo,
        pull_number: prNumber,
        team_reviewers: [DEP_REVIEW_TEAM],
      });
      console.log(`Removed stale review request from @microsoft/${DEP_REVIEW_TEAM}`);
    } catch (e) {
      if (e.status !== 422) {
        console.log(`Note: failed to remove review request (${e.status}): ${e.message}`);
      }
    }
    return;
  }

  // Step 2: Fetch base and PR Cargo.lock via API
  // Always fetch from the base repo — for forked PRs, use the merge ref
  // (refs/pull/N/head) so we don't need access to the fork itself.
  async function fetchFile(path, ref) {
    const { data } = await github.rest.repos.getContent({
      owner: context.repo.owner,
      repo: context.repo.repo,
      path,
      ref,
    });
    if (data.type !== "file") {
      throw new Error(`${path}: not a regular file`);
    }
    return Buffer.from(data.content, "base64").toString("utf8");
  }

  const prRef = `refs/pull/${prNumber}/head`;

  const baseContent = await fetchFile("Cargo.lock", baseSha);
  const prContent = await fetchFile("Cargo.lock", prRef);

  // Step 3: Diff external deps
  const baseDeps = parseExternalDeps(baseContent);
  const prDeps = parseExternalDeps(prContent);
  const diff = diffDeps(baseDeps, prDeps);
  const hasExternalChanges = diff.added.length > 0;

  // Step 4: Check containment policies
  const fs = require("fs");
  const path = require("path");
  const policyPath = path.join(__dirname, "..", "dep-policy.json");
  let newViolations = [];
  if (fs.existsSync(policyPath)) {
    const policy = JSON.parse(fs.readFileSync(policyPath, "utf8"));

    const baseManifest = await fetchFile("Cargo.toml", baseSha);
    const prManifest = await fetchFile("Cargo.toml", prRef);

    const basePathMap = parseCratePathMap(baseManifest);
    const prPathMap = parseCratePathMap(prManifest);

    const baseGraph = parseInternalDepGraph(baseContent);
    const prGraph = parseInternalDepGraph(prContent);

    const baseViolations = checkContainment(
      baseGraph.graph, baseGraph.internalCrates, basePathMap, policy
    );
    const prViolations = checkContainment(
      prGraph.graph, prGraph.internalCrates, prPathMap, policy
    );
    newViolations = diffContainmentViolations(baseViolations, prViolations);
  }

  const needsReview = hasExternalChanges || newViolations.length > 0;

  if (needsReview) {
    // Build and log summary
    let summary = "";
    if (hasExternalChanges) summary += buildSummary(diff);
    if (newViolations.length > 0) summary += buildPolicySummary(newViolations);
    console.log(summary);

    // Request review from the dependency team
    console.log(`Requesting review from @microsoft/${DEP_REVIEW_TEAM}`);
    await github.rest.pulls.requestReviewers({
      owner: context.repo.owner,
      repo: context.repo.repo,
      pull_number: prNumber,
      team_reviewers: [DEP_REVIEW_TEAM],
    });
  } else {
    console.log(
      "Cargo.lock changed, but no new or updated external dependencies " +
        "were detected and no policy violations found. No dependency review required."
    );

    // Remove the review request if it was previously added
    try {
      await github.rest.pulls.removeRequestedReviewers({
        owner: context.repo.owner,
        repo: context.repo.repo,
        pull_number: prNumber,
        team_reviewers: [DEP_REVIEW_TEAM],
      });
      console.log(`Removed review request from @microsoft/${DEP_REVIEW_TEAM}`);
    } catch (e) {
      // 422 = team was not requested — that's fine. Log anything else.
      if (e.status !== 422) {
        console.log(`Note: failed to remove review request (${e.status}): ${e.message}`);
      }
    }
  }
}

// --- Local CLI entrypoint ---

async function localMain() {
  const fs = require("fs");
  const path = require("path");
  const { execSync } = require("child_process");
  const args = process.argv.slice(2);

  if (args.includes("--help") || args.includes("-h")) {
    console.log(
      "Usage:\n" +
        "  node dep-review.js --check              Compare working tree against merge-base with origin/main\n" +
        "  node dep-review.js --check <upstream>    Compare working tree against merge-base with <upstream>\n" +
        "  node dep-review.js --base <file> --pr <file> [--manifest <Cargo.toml> --policy <dep-policy.json>]\n" +
        "\nThe --check mode automatically finds the merge base, reads the base\n" +
        "Cargo.lock/Cargo.toml from git, and compares against the working tree.\n" +
        "It also auto-discovers dep-policy.json relative to the script location.\n" +
        "\nIn --base/--pr mode, --manifest and --policy must be specified together\n" +
        "for containment policy checks (both are optional if you only want dep diff)."
    );
    process.exit(0);
  }

  const checkIdx = args.indexOf("--check");
  let baseContent, prContent, baseManifestContent, prManifestContent;

  if (checkIdx !== -1) {
    // --check mode: auto-detect merge base
    const upstream = args[checkIdx + 1] && !args[checkIdx + 1].startsWith("--")
      ? args[checkIdx + 1]
      : "origin/main";

    // Find repo root
    const root = execSync("git rev-parse --show-toplevel", { encoding: "utf8" }).trim();

    // Find merge base
    let mergeBase;
    try {
      mergeBase = execSync(`git merge-base HEAD ${upstream}`, { encoding: "utf8" }).trim();
    } catch (e) {
      console.error(
        `Could not find merge base with '${upstream}'. ` +
          `Make sure '${upstream}' exists (try: git fetch origin).`
      );
      process.exit(1);
    }

    const shortBase = execSync(`git rev-parse --short ${mergeBase}`, { encoding: "utf8" }).trim();
    console.log(`Comparing working tree against merge base ${shortBase} (with ${upstream})\n`);

    // Read base files from git
    baseContent = execSync(`git show ${mergeBase}:Cargo.lock`, { encoding: "utf8" });
    baseManifestContent = execSync(`git show ${mergeBase}:Cargo.toml`, { encoding: "utf8" });

    // Read PR files from working tree
    prContent = fs.readFileSync(path.join(root, "Cargo.lock"), "utf8");
    prManifestContent = fs.readFileSync(path.join(root, "Cargo.toml"), "utf8");

    // Auto-discover policy file
    const policyPath = path.join(root, ".github", "dep-policy.json");
    let policy = null;
    if (fs.existsSync(policyPath)) {
      policy = JSON.parse(fs.readFileSync(policyPath, "utf8"));
    }

    const result = runChecks(baseContent, prContent, baseManifestContent, prManifestContent, policy);
    process.exit(result ? 1 : 0);

  } else {
    // Explicit --base / --pr mode
    const baseIdx = args.indexOf("--base");
    const prIdx = args.indexOf("--pr");

    if (baseIdx === -1 || prIdx === -1) {
      console.error(
        "Usage:\n" +
          "  node dep-review.js --check              (auto merge-base mode)\n" +
          "  node dep-review.js --base <file> --pr <file> [--manifest <Cargo.toml>] [--policy <dep-policy.json>]"
      );
      process.exit(1);
    }

    baseContent = fs.readFileSync(args[baseIdx + 1], "utf8");
    prContent = fs.readFileSync(args[prIdx + 1], "utf8");

    const manifestIdx = args.indexOf("--manifest");
    const policyIdx = args.indexOf("--policy");

    let policy = null;
    if (manifestIdx !== -1 && policyIdx !== -1) {
      baseManifestContent = fs.readFileSync(args[manifestIdx + 1], "utf8");
      prManifestContent = baseManifestContent; // same manifest in explicit mode
      policy = JSON.parse(fs.readFileSync(args[policyIdx + 1], "utf8"));
    }

    const result = runChecks(baseContent, prContent, baseManifestContent, prManifestContent, policy);
    process.exit(result ? 1 : 0);
  }
}

/**
 * Run all checks (external dep diff + policy). Returns true if issues were found.
 */
function runChecks(baseContent, prContent, baseManifest, prManifest, policy) {
  let failed = false;

  // External dep diff
  const baseDeps = parseExternalDeps(baseContent);
  const prDeps = parseExternalDeps(prContent);
  const diff = diffDeps(baseDeps, prDeps);
  const hasExternalChanges = diff.added.length > 0;

  if (hasExternalChanges) {
    console.log(buildSummary(diff));
    failed = true;
  }

  // Policy checks
  if (policy && baseManifest && prManifest) {
    const basePathMap = parseCratePathMap(baseManifest);
    const prPathMap = parseCratePathMap(prManifest);

    const baseGraph = parseInternalDepGraph(baseContent);
    const prGraph = parseInternalDepGraph(prContent);

    const baseViolations = checkContainment(
      baseGraph.graph, baseGraph.internalCrates, basePathMap, policy
    );
    const prViolations = checkContainment(
      prGraph.graph, prGraph.internalCrates, prPathMap, policy
    );
    const newViolations = diffContainmentViolations(baseViolations, prViolations);

    if (newViolations.length > 0) {
      console.log(buildPolicySummary(newViolations));
      failed = true;
    }
  }

  if (!failed) {
    console.log("No dependency review issues detected.");
  }
  return failed;
}

// Export for testing and for actions/github-script
module.exports = {
  parseExternalDeps,
  fmtSource,
  diffDeps,
  buildSummary,
  parseCratePathMap,
  parseInternalDepGraph,
  checkContainment,
  diffContainmentViolations,
  buildPolicySummary,
  run,
};

// Run CLI if invoked directly
if (require.main === module) {
  localMain();
}
