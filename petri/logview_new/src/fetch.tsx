// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { QueryClient } from "@tanstack/react-query";
import type { RunData, RunMetadata } from "./data_defs";

const GET_RUNS_URL =
  "https://openvmmghtestresults.blob.core.windows.net/results?restype=container&comp=list&showonly=files&include=metadata&prefix=runs/";
const GET_PR_INFO = "https://api.github.com/repos/microsoft/openvmm/pulls/";

/**
 * Start background data prefetching and refetching for the runs list.
 * This ensures the homepage loads instantly and data stays fresh.
 */
export function startDataPrefetching(queryClient: QueryClient): void {
  // Initial prefetch for instant first load
  void queryClient.prefetchQuery({
    queryKey: ["runs"],
    queryFn: () => fetchRunData(queryClient),
    staleTime: 3 * 60 * 1000,
    gcTime: Infinity,
  });

  // Background refetch every 2 minutes to keep data fresh
  setInterval(
    () => {
      void queryClient.refetchQueries({
        queryKey: ["runs"],
        type: "all", // Keeps the runs data current no matter what!
      });
    },
    2 * 60 * 1000
  );
}

// Main export function - fetches and returns parsed run data
export async function fetchRunData(
  queryClient: QueryClient
): Promise<RunData[]> {
  try {
    const response = await fetch(GET_RUNS_URL);
    const data = await response.text();

    // Parse the data and get the runs array
    const runs = parseRunData(data, queryClient);

    // Collect all PR numbers that need titles
    const prNumbers = runs
      .map((run) => run.metadata.ghPr)
      .filter((pr): pr is string => pr !== undefined);

    if (prNumbers.length > 0) {
      // Use per-PR cached queries (never stale, never garbage collected) to
      // avoid redundant network calls.
      // NOTE: PR titles will not be updated even if they are updated in the
      // back end. This saves us from hitting GitHub's rate limits. Could be
      // rethought to pull stuff after 15min.
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
      const titleMap = new Map<string, string | null>(entries);
      runs.forEach((run) => {
        const pr = run.metadata.ghPr;
        if (pr && titleMap.has(pr)) {
          const t = titleMap.get(pr);
          if (t) run.metadata.prTitle = t;
        }
      });
    }

    return runs;
  } catch (error) {
    console.error("Error fetching run data:", error);
    throw error;
  }
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

// Function to parse XML run data into structured format
function parseRunData(xmlText: string, queryClient: QueryClient): RunData[] {
  const parser = new DOMParser();
  const xmlDoc = parser.parseFromString(xmlText, "text/xml");

  // Parse each blob
  const blobs = xmlDoc.getElementsByTagName("Blob");
  const runs: RunData[] = [];

  for (const blob of blobs) {
    const name = blob.getElementsByTagName("Name")[0]?.textContent || "";
    const creationTime = new Date(
      blob.getElementsByTagName("Creation-Time")[0]?.textContent || ""
    );
    const lastModified = new Date(
      blob.getElementsByTagName("Last-Modified")[0]?.textContent || ""
    );
    const etag = blob.getElementsByTagName("Etag")[0]?.textContent || "";
    const contentLength = parseInt(
      blob.getElementsByTagName("Content-Length")[0]?.textContent || "0"
    );

    // Parse metadata
    const metadataElement = blob.getElementsByTagName("Metadata")[0];
    const metadata: RunMetadata = {
      petriFailed: parseInt(
        metadataElement?.getElementsByTagName("petrifailed")[0]?.textContent ||
          "0"
      ),
      petriPassed: parseInt(
        metadataElement?.getElementsByTagName("petripassed")[0]?.textContent ||
          "0"
      ),
      ghBranch:
        metadataElement?.getElementsByTagName("ghbranch")[0]?.textContent || "",
      ghPr:
        metadataElement?.getElementsByTagName("ghpr")[0]?.textContent ||
        undefined,
    };

    runs.push({
      name,
      creationTime,
      lastModified,
      etag,
      contentLength,
      metadata,
    });
  }

  // TODO: Trigger background data prefetching of runDetails in future PR.
  // opportunisticPrefetching(runs, queryClient);
  return runs;
}
