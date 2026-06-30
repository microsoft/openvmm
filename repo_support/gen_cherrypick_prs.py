#!/usr/bin/env python3

# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

# Generate 1 cherry-pick PR per merged (squash) PR, targeting a release branch,
# processing PRs in the same order they were merged (by mergedAt).

# Behavior:
# - Creates a new branch from the release branch for each PR
# - Cherry-picks the squash merge commit (mergeCommit.oid)
# - Pushes the branch
# - Shows the exact PR title/body it will use (same as original PR body, with a prefix line)
# - ASKS FOR CONFIRMATION before creating the PR on GitHub
# - STOPS immediately if cherry-pick hits conflicts (no PR creation)

# Requires:
#   - git
#   - GitHub CLI (gh) authenticated

# Examples:
#     # Backport specific merged PRs, sorted by merge order, to release/1.7.2511.
#     gen_cherrypick_prs.py release/1.7.2511 2567 2525 2533 2550 2551 2602

#     # Auto-load all PRs labeled backport_<release> on main and backport only the
#     # merged ones; open/closed entries are listed as not completed/abandoned.
#     #
#     # This example is for the 1.7.2511 release.
#     gen_cherrypick_prs.py --from-backport-label release/1.7.2511

from __future__ import annotations

import argparse
import json
import re
import shlex
import subprocess
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import List, Optional, Tuple


@dataclass(frozen=True)
class PRInfo:
    number: int
    title: str
    body: str
    url: str
    merged_at: datetime
    merge_sha: str
    state: str


@dataclass(frozen=True)
class BackportInfo:
    state: str  # OPEN, MERGED, CLOSED
    url: str
    number: int
    merged_at: Optional[datetime]


def run(cmd: List[str], *, check: bool = True, cwd: Optional[str] = None) -> str:
    """Run a command and return stdout (stripped). Raises on failure if check=True."""
    p = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, check=False, cwd=cwd)

    if check and p.returncode != 0:
        stderr = p.stderr or ""
        stdout = p.stdout or ""
        raise RuntimeError(
            f"Command failed ({p.returncode}): {shlex.join(cmd)}\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}"
        )

    return (p.stdout or "").strip()


def ensure_clean_worktree(allow_dirty: bool) -> None:
    status = run(["git", "status", "--porcelain"])
    if status and not allow_dirty:
        raise SystemExit(
            "Working tree is not clean. Commit/stash changes or re-run with --allow-dirty."
        )


def parse_pr_numbers(pr_args: List[str]) -> List[int]:
    nums: List[int] = []
    for item in pr_args:
        s = item.strip()
        # Accept "123", "#123", or URL containing "/pull/123"
        if "/pull/" in s:
            m = re.search(r"/pull/(\d+)", s)
        else:
            m = re.search(r"(\d+)$", s.lstrip("#"))
        if not m:
            raise SystemExit(f"Could not parse PR number from: {item}")
        nums.append(int(m.group(1)))
    # Dedup while preserving order
    seen = set()
    out = []
    for n in nums:
        if n not in seen:
            out.append(n)
            seen.add(n)
    return out


def parse_github_datetime(s: str) -> datetime:
    # GitHub typically returns ISO8601 like "2026-01-21T22:15:03Z"
    if s.endswith("Z"):
        s = s[:-1] + "+00:00"
    dt = datetime.fromisoformat(s)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt.astimezone(timezone.utc)


def find_backport_infos(
    pr_number: int,
    base_branch: str,
    repo: Optional[str],
    orig_title: str,
) -> List[BackportInfo]:
    # Heuristic: look for PRs targeting the release branch that reference the original PR by number,
    # URL, or title. We search in multiple ways and then filter locally.
    queries = [
        f'base:{base_branch} "PR #{pr_number}" in:title,body',
        f'base:{base_branch} "pull/{pr_number}" in:body',
    ]
    if orig_title:
        queries.append(f'base:{base_branch} "{orig_title}" in:title')

    items: List[dict] = []
    seen_numbers: Set[int] = set()
    for q in queries:
        for pr in gh_pr_list_search(q, repo):
            n = int(pr.get("number")) if pr.get("number") is not None else None
            if n is None or n in seen_numbers:
                continue
            seen_numbers.add(n)
            items.append(pr)

    rx = re.compile(rf"\bPR\s*#?{re.escape(str(pr_number))}(?:\b|$)", re.IGNORECASE)
    url_fragment = f"/pull/{pr_number}"
    title_lc = orig_title.lower()

    matches: List[BackportInfo] = []
    for pr in items:
        title = str(pr.get("title") or "")
        body = str(pr.get("body") or "")
        hay = f"{title}\n{body}"
        if not (
            rx.search(hay)
            or url_fragment in hay
            or (title_lc and title_lc in title.lower())
            or f"(#{pr_number})" in title
        ):
            continue
        merged_at_raw = pr.get("mergedAt") or ""
        merged_at = parse_github_datetime(merged_at_raw) if merged_at_raw else None
        matches.append(
            BackportInfo(
                state=str(pr.get("state") or "").upper(),
                url=str(pr.get("url") or "").strip(),
                number=int(pr.get("number")),
                merged_at=merged_at,
            )
        )

    # Sort merged first (most recent), then open, then closed
    merged = sorted(
        [m for m in matches if m.state == "MERGED"],
        key=lambda x: x.merged_at or datetime.min.replace(tzinfo=timezone.utc),
        reverse=True,
    )
    opened = [m for m in matches if m.state == "OPEN"]
    closed = [m for m in matches if m.state not in ("MERGED", "OPEN")]
    return merged + opened + closed


def gh_pr_view(pr_number: int, repo: Optional[str]) -> dict:
    # We explicitly request body/title/mergedAt/mergeCommit so we can reuse title/body verbatim.
    cmd = [
        "gh", "pr", "view", str(pr_number),
        "--json", "number,title,body,url,state,mergedAt,mergeCommit"
    ]
    if repo:
        cmd.extend(["-R", repo])
    out = run(cmd)
    return json.loads(out)


def gh_pr_list_search(query: str, repo: Optional[str]) -> List[dict]:
    cmd = [
        "gh",
        "pr",
        "list",
        "--state",
        "all",
        "--search",
        query,
        "--limit",
        "500",
        "--json",
        "number,title,body,url,state,mergedAt",
    ]
    if repo:
        cmd.extend(["-R", repo])
    out = run(cmd)
    if not out.strip():
        return []
    return json.loads(out)


def gh_pr_list_label(label: str, repo: Optional[str]) -> List[dict]:
    cmd = [
        "gh",
        "pr",
        "list",
        "--state",
        "all",
        "--base",
        "main",
        "--label",
        label,
        "--limit",
        "1000",
        "--json",
        "number,title,body,url,state,mergedAt",
    ]
    if repo:
        cmd.extend(["-R", repo])
    out = run(cmd)
    if not out.strip():
        return []
    return json.loads(out)


def extract_merge_sha(prj: dict) -> str:
    """
    mergeCommit is commonly an object with an 'oid' field.
    We'll try common shapes defensively.
    """
    mc = prj.get("mergeCommit")
    if mc is None:
        return ""
    if isinstance(mc, str):
        return mc.strip()
    if isinstance(mc, dict):
        for k in ("oid", "sha", "id"):
            v = mc.get(k)
            if isinstance(v, str) and v.strip():
                return v.strip()
    return ""


def git_fetch(remote: str) -> None:
    run(["git", "fetch", remote], check=True)


def git_checkout_branch_from(remote: str, base_branch: str, new_branch: str) -> None:
    # Create/reset local branch to remote/base
    run(["git", "checkout", "-B", new_branch, f"{remote}/{base_branch}"], check=True)


def git_cherrypick_x(sha: str) -> None:
    # -x appends "(cherry picked from commit ...)" to the commit message
    run(["git", "cherry-pick", "-x", sha], check=True)


def git_commit_subject(sha: str) -> str:
    return run(["git", "show", "-s", "--format=%s", sha], check=True)


def git_push(remote: str, branch: str, force: bool) -> None:
    cmd = ["git", "push", "-u", remote, branch]
    if force:
        cmd.insert(2, "--force-with-lease")
    try:
        run(cmd, check=True)
        return
    except RuntimeError as e:
        msg = str(e)
        if force:
            raise
        if "non-fast-forward" in msg or "non fast-forward" in msg:
            if confirm(
                f"Remote branch '{branch}' is ahead on {remote}. Force-with-lease push? [y/N] "
            ):
                cmd = ["git", "push", "-u", "--force-with-lease", remote, branch]
                run(cmd, check=True)
                return
        raise


def gh_pr_create(
    repo: Optional[str],
    base: str,
    head: str,
    title: str,
    body: str,
    draft: bool,
) -> str:
    cmd = ["gh", "pr", "create", "--base", base, "--head", head, "--title", title, "--body", body]
    if draft:
        cmd.append("--draft")
    if repo:
        cmd.extend(["-R", repo])
    # gh pr create prints the URL of the created PR on success.
    return run(cmd, check=True)


def git_remote_url(remote: str) -> str:
    return run(["git", "remote", "get-url", remote], check=True)


def parse_github_owner_repo(remote_url: str) -> Optional[str]:
    # Supports: https://github.com/OWNER/REPO(.git) and git@github.com:OWNER/REPO(.git)
    url = remote_url.strip()
    if url.startswith("git@github.com:"):
        path = url[len("git@github.com:") :]
    elif url.startswith("https://github.com/"):
        path = url[len("https://github.com/") :]
    else:
        return None
    path = path.rstrip("/")
    if path.endswith(".git"):
        path = path[:-4]
    if path.count("/") != 1:
        return None
    return path


def confirm(prompt: str) -> bool:
    # Simple interactive confirmation
    resp = input(prompt).strip().lower()
    return resp in ("y", "yes")


def main() -> int:
    ap = argparse.ArgumentParser(
        description="Generate cherry-pick PRs (one per merged PR) targeting a release branch, sorted by mergedAt."
    )
    ap.add_argument("release_branch", help="Name of the release branch (target/base) e.g. release/1.7")
    ap.add_argument("prs", nargs="*", help="PR numbers (e.g. 123) or #123 or PR URLs")
    ap.add_argument("--repo", "-R", default=None, help="Optional: OWNER/REPO. If omitted, uses current repo context.")
    ap.add_argument("--base-remote", default="upstream", help="Remote for the base branch (default: upstream)")
    ap.add_argument("--push-remote", default="origin", help="Remote to push cherry-pick branches (default: origin)")
    ap.add_argument("--branch-prefix", default="cherrypick", help="Prefix for new branches (default: cherrypick)")
    ap.add_argument("--draft", action="store_true", help="Create the cherry-pick PRs as draft PRs")
    ap.add_argument("--allow-dirty", action="store_true", help="Do not require a clean git working tree")
    ap.add_argument("--force-push", action="store_true", help="Force-with-lease push if branch already exists remotely")
    ap.add_argument("--dry-run", action="store_true", help="Print what would happen, but do not modify git or create PRs")
    ap.add_argument(
        "--from-backport-label",
        action="store_true",
        help="Load PRs from the backport_<release> label on main instead of passing PR numbers",
    )

    args = ap.parse_args()

    ensure_clean_worktree(args.allow_dirty)

    if not args.from_backport_label and not args.prs:
        raise SystemExit("Must provide PR numbers, or use --from-backport-label.")

    pr_numbers = parse_pr_numbers(args.prs) if args.prs else []

    infos: List[PRInfo] = []
    not_completed: List[PRInfo] = []

    if args.from_backport_label:
        release_name = args.release_branch
        m = re.match(r"^release/(.+)$", args.release_branch)
        if m:
            release_name = m.group(1)
        label = f"backport_{release_name}"
        labeled = gh_pr_list_label(label, args.repo)
        for prj in labeled:
            state = str(prj.get("state") or "").upper()
            num = int(prj.get("number"))
            title = str(prj.get("title") or "").rstrip()
            url = str(prj.get("url") or "").strip()
            if state == "MERGED":
                full = gh_pr_view(num, args.repo)
                merged_at_raw = full.get("mergedAt") or ""
                if not merged_at_raw:
                    raise SystemExit(f"PR #{num} has no mergedAt. Aborting.")
                merged_at = parse_github_datetime(merged_at_raw)
                merge_sha = extract_merge_sha(full)
                if not merge_sha:
                    raise SystemExit(f"PR #{num} has no merge commit SHA (mergeCommit missing). Aborting.")
                infos.append(
                    PRInfo(
                        number=num,
                        title=title,
                        body=str(full.get("body") or "").rstrip(),
                        url=url,
                        merged_at=merged_at,
                        merge_sha=merge_sha,
                        state=state,
                    )
                )
            else:
                not_completed.append(
                    PRInfo(
                        number=num,
                        title=title,
                        body=str(prj.get("body") or "").rstrip(),
                        url=url,
                        merged_at=datetime.min.replace(tzinfo=timezone.utc),
                        merge_sha="",
                        state=state,
                    )
                )
    else:
        # Gather PR info first so we can sort by mergedAt.
        for n in pr_numbers:
            prj = gh_pr_view(n, args.repo)

            state = str(prj.get("state", "")).upper()
            if state != "MERGED":
                raise SystemExit(f"PR #{n} is not merged (state={state}). Aborting.")

            merged_at_raw = prj.get("mergedAt") or ""
            if not merged_at_raw:
                raise SystemExit(f"PR #{n} has no mergedAt. Aborting.")
            merged_at = parse_github_datetime(merged_at_raw)

            merge_sha = extract_merge_sha(prj)
            if not merge_sha:
                raise SystemExit(f"PR #{n} has no merge commit SHA (mergeCommit missing). Aborting.")

            infos.append(
                PRInfo(
                    number=int(prj["number"]),
                    title=str(prj.get("title") or "").rstrip(),
                    body=str(prj.get("body") or "").rstrip(),
                    url=str(prj.get("url") or "").strip(),
                    merged_at=merged_at,
                    merge_sha=merge_sha,
                    state=state,
                )
            )

    infos.sort(key=lambda x: x.merged_at)  # same order they were merged
    not_completed.sort(key=lambda x: x.number)

    if args.dry_run:
        print("--dry-run set; no changes will be made.")
        if infos:
            print("The following PRs would be processed:")
            for info in infos:
                short_sha = info.merge_sha[:8] if info.merge_sha else ""
                if short_sha:
                    print(f"  #{info.number} {info.title} ({short_sha})")
                else:
                    print(f"  #{info.number} {info.title}")
        if not_completed:
            print("The following PRs were skipped because they are not yet completed:")
            for info in not_completed:
                print(f"  #{info.number} {info.title}")
        return 0

    # Make sure we have the latest release branch
    git_fetch(args.base_remote)

    base_repo = args.repo
    base_remote_repo = None
    push_remote_repo = None
    try:
        base_remote_repo = parse_github_owner_repo(git_remote_url(args.base_remote))
    except Exception:
        base_remote_repo = None
    try:
        push_remote_repo = parse_github_owner_repo(git_remote_url(args.push_remote))
    except Exception:
        push_remote_repo = None

    if base_repo is None:
        base_repo = base_remote_repo
    if base_repo is None:
        print(
            "Error: Could not determine base repo from git remotes."
            "Pass --repo OWNER/REPO or run from within a GitHub repository.",
            file=sys.stderr,
        )
        raise SystemExit("Could not determine base repo from git remotes.")

    green = "\x1b[32m"
    orange = "\x1b[38;5;208m"
    reset = "\x1b[0m"

    def status_label(state: str) -> str:
        if state == "MERGED":
            return f"{green}*BACKPORTED*{reset}"
        if state == "OPEN":
            return f"{orange}*IN PROGRESS*{reset}"
        return "*NONE*"

    backport_map: Dict[int, List[BackportInfo]] = {}
    for info in infos:
        backport_map[info.number] = find_backport_infos(
            info.number, args.release_branch, base_repo, info.title
        )

    print("Will process PRs in merged order:")
    for i, info in enumerate(infos, 1):
        bps = backport_map.get(info.number) or []
        print(f"  {i:02d}. #{info.number} mergedAt={info.merged_at.isoformat()} sha={info.merge_sha}")
        print(f"      title: {info.title}")
        if not bps:
            print("      backport: *NONE*")
        else:
            if len(bps) > 1:
                print(f"      backport: *MULTIPLE* ({len(bps)})")
            for bp in bps:
                label = status_label(bp.state)
                print(f"        {label} {bp.url}")

    if not_completed:
        print("\nPending merge into main:")
        for info in not_completed:
            status = "pending merge into main" if info.state == "OPEN" else "abandoned"
            print(f"  #{info.number} ({info.state.lower()}): {status}")
            print(f"      title: {info.title}")
            print(f"      {info.url}")

    created: List[Tuple[int, str]] = []

    for info in infos:
        bps = backport_map.get(info.number) or []
        active = [bp for bp in bps if bp.state in ("OPEN", "MERGED")]
        if active:
            if len(active) == 1:
                bp = active[0]
                print(
                    f"Skipping PR #{info.number} (backport {bp.state.lower()}): {bp.url}"
                )
            else:
                links = ", ".join(bp.url for bp in active)
                print(
                    f"Skipping PR #{info.number} (multiple backports in progress/merged): {links}"
                )
            continue
        rel_sanitized = re.sub(r"[^A-Za-z0-9._/-]+", "-", args.release_branch).strip("-")
        branch = f"{args.branch_prefix}/{rel_sanitized}/pr-{info.number}"

        print(f"\n=== Cherry-pick PR #{info.number} -> {args.release_branch} ===")
        print(f"Branch: {branch}")
        print(f"Original PR: {info.url}")
        print(f"Cherry-pick commit: {info.merge_sha}")

        # Create branch from release and cherry-pick
        git_checkout_branch_from(args.base_remote, args.release_branch, branch)

        print("Cherry-picking...")
        try:
            git_cherrypick_x(info.merge_sha)
        except RuntimeError as e:
            # Stop immediately on conflicts or any cherry-pick failure.
            print(str(e), file=sys.stderr)
            print("\nSTOPPING: Cherry-pick failed (likely conflicts). No PR will be created.", file=sys.stderr)
            print("Resolve manually if you want, then you can re-run for remaining PRs.", file=sys.stderr)
            return 2

        print(f"Pushing branch to {args.push_remote} ...")
        git_push(args.push_remote, branch, args.force_push)

        head_ref = branch
        if base_repo and push_remote_repo and base_repo != push_remote_repo:
            fork_owner = push_remote_repo.split("/")[0]
            head_ref = f"{fork_owner}:{branch}"

        # Use the first line of the merge commit, ensuring it includes the original PR number.
        base_title = git_commit_subject(info.merge_sha).rstrip()
        if re.search(r"\(#\d+\)\s*$", base_title):
            pr_title = base_title
        else:
            pr_title = f"{base_title} (#{info.number})"
        pr_body = f"Clean cherry pick of PR #{info.number}\n\n{info.body}".rstrip() + "\n"

        print("\n--- PR to be created ---")
        print(f"Base: {args.release_branch}")
        print(f"Head: {head_ref}")
        print(f"Title: {pr_title}")
        print("Body (first ~20 lines):")
        preview_lines = pr_body.splitlines()[:20]
        for line in preview_lines:
            print(line)
        if len(pr_body.splitlines()) > 20:
            print("...")

        if not confirm("Create this PR on GitHub? [y/N] "):
            print("Aborting by user request. No further PRs will be created.")
            return 0

        print("Creating PR on GitHub...")
        pr_url = gh_pr_create(
            repo=base_repo,
            base=args.release_branch,
            head=head_ref,
            title=pr_title,
            body=pr_body,
            draft=args.draft,
        )
        created.append((info.number, pr_url))
        print(f"Created: {pr_url}")

    print("\nDone. Created PRs:")
    for n, url in created:
        print(f"  #{n}: {url}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
