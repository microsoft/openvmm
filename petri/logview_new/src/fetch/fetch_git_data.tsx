// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { QueryClient } from "@tanstack/react-query";
import { PullRequestTitles } from "../data_defs";

const GET_PR_INFO = "https://api.github.com/repos/microsoft/openvmm/pulls/";
const GET_PR_INFO_BATCHED =
  "https://api.github.com/repos/microsoft/openvmm/pulls?state=all&sort=updated&direction=desc";
const PER_PAGE = 50;

/**
 * Gets up to 200 of the most recent PRs from the openvmm repo. Parses them and
 * returns a mapping from PR number -> PR title. Returns an empty object if
 * there are failures.
 */
export async function getAllGithubPullRequests(): Promise<PullRequestTitles> {
  try {
    // Fetch first four pages in parallel. Each page returns up to 50 PRs.
    const pagePromises = [1, 2, 3, 4].map((p) =>
      getGithubPullRequestsPage(p, PER_PAGE).catch(
        () => ({}) as PullRequestTitles
      )
    );
    const pageMaps = await Promise.all(pagePromises);
    // Merge (later pages won't overwrite earlier ones if duplicate numbers appear, but duplicates are unlikely)
    const merged: PullRequestTitles = {};
    for (const m of pageMaps) {
      for (const k in m) {
        if (!Object.prototype.hasOwnProperty.call(merged, k)) {
          merged[k] = m[k];
        }
      }
    }
    return merged;
  } catch (e) {
    console.warn("[getGithubPullRequests] Failed to aggregate pages", e);
    return {};
  }
}

/**
 * Fetch a page of pull requests from microsoft/openvmm and build a mapping
 * from PR number (as string) -> PR title. Returns an empty object on any failure so that
 * prefetch errors never block the UI.
 */
async function getGithubPullRequestsPage(
  number: number,
  perPage: number
): Promise<PullRequestTitles> {
  const url = `${GET_PR_INFO_BATCHED}&per_page=${perPage}&page=${number}`;
  try {
    const res = await fetch(url, {
      headers: {
        Accept: "application/vnd.github+json",
      },
    });
    if (!res.ok) {
      console.warn(
        "[getGithubPullRequests] Non-OK response"
      );
      return {};
    }
    const data = await res.json();
    if (!Array.isArray(data)) return {};
    const map: PullRequestTitles = {};
    for (const pr of data) {
      if (pr && typeof pr.title === "string" && typeof pr.number === "number") {
        map[pr.number] = pr.title;
      }
    }
    return map;
  } catch (e) {
    console.warn("[getGithubPullRequests] Failed to fetch PRs", e);
    return {};
  }
}

/**
 * Fetch missing PR titles for PRs provided in the prNumber list.
 * Uses per-PR cached queries (never stale, never garbage collected) to
 * avoid redundant network calls.
 */
export async function fetchMissingPRTitles(
  prNumbers: string[],
  queryClient: QueryClient
): Promise<Map<string, string | null>> {
  console.warn(
    "[fetchMissingPRTitles] Fetching missing PR titles individually:",
    prNumbers
  );
  const unique = Array.from(new Set(prNumbers));
  const entries = await Promise.all(
    unique.map(async (pr) => {
      const title = await queryClient.ensureQueryData<string | null>({
        queryKey: ["prTitle", pr],
        queryFn: () => fetchSinglePRTitle(pr),
        staleTime: Infinity, // Never goes stale
        gcTime: Infinity, // Never garbage collected
      });
      return [pr, title] as const;
    })
  );
  return new Map<string, string | null>(entries);
}

/** Fetch a single PR title from GitHub. Returns null if unavailable or rate-limited. */
async function fetchSinglePRTitle(prNumber: string): Promise<string | null> {
  try {
    const response = await fetch(`${GET_PR_INFO}${prNumber}`);
    if (response.status === 403) {
      // Likely rate limited – treat as missing but keep cached null to avoid hammering.
      return null;
    }
    if (response.ok) {
      const prData = await response.json();
      return typeof prData.title === "string" ? prData.title : null;
    }
  } catch {
    /* swallow network errors; null indicates unknown */
  }
  return null;
}
