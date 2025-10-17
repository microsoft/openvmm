// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { QueryClient } from "@tanstack/react-query";
import type {
  RunData,
  RunDetailsData,
  RunMetadata,
  TestResult,
} from "./data_defs";

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
  opportunisticPrefetching(runs, queryClient);
  return runs;
}

/**
 * We avoid duplicate work and run this in the background so initial render isn't blocked.
 * Prefetches in batches of 5 concurrent requests to balance speed vs resource usage.
 */
function opportunisticPrefetching(
  runs: RunData[],
  queryClient: QueryClient
): void {
  try {
    // Sort all runs by creation time descending
    const sortedRuns = [...runs].sort(
      (a, b) => b.creationTime.getTime() - a.creationTime.getTime()
    );

    const extractRunNumber = (name: string) => {
      const runNumberFull = name.replace(/^runs\//, "");
      return runNumberFull.split("/")[0];
    };

    void (async () => {
      const prefetched = new Set<string>();
      const prefetchList: string[] = [];

      // Step 1: First 7 failed runs
      const failedRuns = sortedRuns.filter((r) => r.metadata.petriFailed > 0);
      const first7Failed = failedRuns.slice(0, 7);
      for (const run of first7Failed) {
        const runNumber = extractRunNumber(run.name);
        if (runNumber) {
          prefetchList.push(runNumber);
          prefetched.add(runNumber);
        }
      }

      // Step 2: Top 10 runs overall (regardless of status/branch)
      const top10 = sortedRuns.slice(0, 10);
      for (const run of top10) {
        const runNumber = extractRunNumber(run.name);
        if (runNumber && !prefetched.has(runNumber)) {
          prefetchList.push(runNumber);
          prefetched.add(runNumber);
        }
      }

      // Step 3: Last 7 runs on main branch
      const mainRuns = sortedRuns
        .filter((r) => r.metadata.ghBranch === "main")
        .slice(0, 7);
      for (const run of mainRuns) {
        const runNumber = extractRunNumber(run.name);
        if (runNumber && !prefetched.has(runNumber)) {
          prefetchList.push(runNumber);
          prefetched.add(runNumber);
        }
      }

      // Prefetch with controlled parallelism (5 concurrent requests at a time)
      const BATCH_SIZE = 5;
      const prefetchRun = async (runNumber: string) => {
        const key = ["runDetails", runNumber];
        if (queryClient.getQueryData(key)) return;
        try {
          await queryClient.prefetchQuery({
            queryKey: key,
            queryFn: () => fetchRunDetails(runNumber, queryClient),
            staleTime: Infinity,
            gcTime: Infinity,
          });
        } catch (e) {
          console.warn(
            `[opportunisticPrefetching] Prefetch failed for run ${runNumber}`,
            e
          );
        }
      };

      // Process in batches to limit concurrent requests
      for (let i = 0; i < prefetchList.length; i += BATCH_SIZE) {
        const batch = prefetchList.slice(i, i + BATCH_SIZE);
        await Promise.allSettled(
          batch.map((runNumber) => prefetchRun(runNumber))
        );
      }
    })();
  } catch (e) {
    console.warn(
      "[opportunisticPrefetching] Failed to schedule runDetails prefetch",
      e
    );
  }
}

// Function to parse detailed run data from XML using lightweight regex parsing
function parseRunDetails(
  xmlText: string,
  runNumber: string,
  queryClient: QueryClient
): RunDetailsData {
  const testFolders = new Map<
    string,
    { hasJsonl: boolean; hasPassed: boolean }
  >();

  // Extract creation time from the first blob
  let creationTime: Date | null = null;
  try {
    const creationTimeMatch = xmlText.match(
      /<Creation-Time>([^<]+)<\/Creation-Time>/
    );
    if (creationTimeMatch) {
      const parsedDate = new Date(creationTimeMatch[1]);
      if (!isNaN(parsedDate.getTime())) {
        creationTime = parsedDate;
      }
    }
  } catch {
    // If parsing fails, creationTime remains null
  }

  // Regex to extract Name elements from Blob entries
  // This avoids creating a full DOM tree and just scans the text
  const nameRegex = /<Name>([^<]+)<\/Name>/g;

  let match;
  while ((match = nameRegex.exec(xmlText)) !== null) {
    const name = match[1];
    const nameParts = name.split("/");
    const fileName = nameParts[nameParts.length - 1];

    // Skip if not a test result file
    if (fileName !== "petri.jsonl" && fileName !== "petri.passed") {
      continue;
    }

    // Extract test folder path (everything except the filename)
    const testFolderPath = nameParts.slice(0, -1).join("/");

    // Initialize or update the test folder tracking
    if (!testFolders.has(testFolderPath)) {
      testFolders.set(testFolderPath, { hasJsonl: false, hasPassed: false });
    }

    const folder = testFolders.get(testFolderPath)!;
    if (fileName === "petri.jsonl") {
      folder.hasJsonl = true;
    } else if (fileName === "petri.passed") {
      folder.hasPassed = true;
    }
  }

  // Second pass: create test results based on the logic from old implementation
  const tests: TestResult[] = [];

  for (const [testFolderPath, folder] of testFolders) {
    // Only process folders that have petri.jsonl (these are test result folders)
    if (!folder.hasJsonl) {
      continue;
    }

    const pathParts = testFolderPath.split("/");

    // The path structure should be: runNumber/architecture/jobName/testName
    // Since runNumber is just the number, we need to remove it from the path
    if (pathParts.length >= 2) {
      // Remove the run number prefix from the path parts
      const cleanPathParts = pathParts.slice(1); // Skip the first part which is the run number

      if (cleanPathParts.length >= 2) {
        // Now we have: architecture/jobName/testName (or more levels)
        const architecture = cleanPathParts[0];
        const testName = cleanPathParts.slice(1).join("/"); // Everything after architecture

        // Determine status: if folder has petri.passed, it's passed; otherwise failed
        const status: "passed" | "failed" = folder.hasPassed
          ? "passed"
          : "failed";

        // Create a clean test name that includes architecture for grouping
        const fullTestName = `${architecture}/${testName}`;

        tests.push({
          name: fullTestName,
          status,
          path: testFolderPath,
        });
      }
    }
  }

  // Sort tests by name
  tests.sort((a, b) => a.name.localeCompare(b.name));

  return {
    creationTime: creationTime ?? undefined,
    runNumber,
    tests,
  };
}

/**
 * Fetch detailed run information (listing of test result folders) for a run number.
 * When a QueryClient is supplied we proactively prefetch & cache the content of
 * any petri.jsonl (and petri.passed) files discovered during the blob listing.
 */
export async function fetchRunDetails(
  runNumber: string,
  queryClient: QueryClient
): Promise<RunDetailsData> {
  try {
    let allTests: TestResult[] = [];
    let continuationToken: string | null = null;
    let creationTime: Date | null = null;

    do {
      // Build URL with continuation token if we have one
      // TODO: If heirarchical namespaces are supported this fetch call might go by much faster. Try this out in a non-prod environment first to try it out
      let url = `https://openvmmghtestresults.blob.core.windows.net/results?restype=container&comp=list&showonly=files&prefix=${encodeURIComponent(runNumber)}`;
      if (continuationToken) {
        url += `&marker=${encodeURIComponent(continuationToken)}`;
      }

      const response = await fetch(url);
      if (!response.ok) {
        throw new Error(
          `Failed to fetch run details: ${response.status} ${response.statusText}`
        );
      }

      const data = await response.text();
      const pageResults = parseRunDetails(data, runNumber, queryClient);

      if (!creationTime && pageResults.creationTime) {
        creationTime = pageResults.creationTime;
      }

      // Merge tests from this page
      allTests.push(...pageResults.tests);

      // Check for NextMarker using regex instead of DOMParser (more memory efficient)
      const nextMarkerMatch = data.match(/<NextMarker>([^<]+)<\/NextMarker>/);
      continuationToken = nextMarkerMatch ? nextMarkerMatch[1] : null;
    } while (continuationToken);

    // Sort all tests by name
    allTests.sort((a, b) => a.name.localeCompare(b.name));
    return {
      creationTime: creationTime ?? undefined,
      runNumber,
      tests: allTests,
    };
  } catch (error) {
    console.error(`Error fetching run details for ${runNumber}:`, error);
    throw error;
  }
}
