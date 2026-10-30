// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { QueryClient } from "@tanstack/react-query";
import type {
  RunData,
  RunDetailsData,
  RunMetadata,
  TestResult,
  PullRequestTitles,
  TestRunInfo,
  TestData,
} from "../data_defs";
import {
  fetchMissingPRTitles,
  getAllGithubPullRequests,
} from "./fetch_git_data";

const GET_RUNS_URL =
  "https://openvmmghtestresults.blob.core.windows.net/results?restype=container&comp=list&showonly=files&include=metadata&prefix=attempts/";

// Main export function - fetches and returns parsed run data
export async function fetchRunData(
  queryClient: QueryClient,
): Promise<RunData[]> {
  try {
    const response = await fetch(GET_RUNS_URL);
    const data = await response.text();

    // Parse the data and get the runs array
    const runs = parseRunData(data);

    // Collect all PR numbers that need titles
    const prNumbers = runs
      .map((run) => run.metadata.ghPr)
      .filter((pr): pr is string => pr !== undefined);

    if (prNumbers.length > 0) {
      // NOTE: We could make this refresh every hour to keep PR titles fresh.
      // But this is fine for now! Titles will currently not be updated after
      // initial fetch.
      const prMap = await queryClient.ensureQueryData<PullRequestTitles>({
        queryKey: ["prs"],
        queryFn: () => getAllGithubPullRequests(),
        staleTime: Infinity,
        gcTime: Infinity,
      });

      // Track missing PR numbers that aren't in the bulk-fetched map
      const missingPRs: string[] = [];

      runs.forEach((run) => {
        const prNumber = run.metadata.ghPr;
        if (prNumber) {
          if (prMap[prNumber]) {
            run.metadata.prTitle = prMap[prNumber];
          } else {
            missingPRs.push(prNumber);
          }
        }
      });

      // Fetch missing PR titles using individual API calls
      if (missingPRs.length > 0) {
        const missingTitles = await fetchMissingPRTitles(
          missingPRs,
          queryClient,
        );
        runs.forEach((run) => {
          const pr = run.metadata.ghPr;
          if (pr && missingTitles.has(pr)) {
            const title = missingTitles.get(pr);
            if (title) run.metadata.prTitle = title;
          }
        });
      }
    }

    return runs;
  } catch (error) {
    console.error("Error fetching run data:", error);
    throw error;
  }
}

// Function to parse XML run data into structured format
function parseRunData(xmlText: string): RunData[] {
  const parser = new DOMParser();
  const xmlDoc = parser.parseFromString(xmlText, "text/xml");

  // Parse each blob
  const blobs = xmlDoc.getElementsByTagName("Blob");
  const runs: RunData[] = [];

  for (const blob of blobs) {
    const name = blob.getElementsByTagName("Name")[0]?.textContent || "";
    const creationTime = new Date(
      blob.getElementsByTagName("Creation-Time")[0]?.textContent || "",
    );
    const lastModified = new Date(
      blob.getElementsByTagName("Last-Modified")[0]?.textContent || "",
    );
    const etag = blob.getElementsByTagName("Etag")[0]?.textContent || "";
    const contentLength = parseInt(
      blob.getElementsByTagName("Content-Length")[0]?.textContent || "0",
    );

    // Parse metadata
    const metadataElement = blob.getElementsByTagName("Metadata")[0];
    const metadata: RunMetadata = {
      petriFailed: parseInt(
        metadataElement?.getElementsByTagName("petrifailed")[0]?.textContent ||
          "0",
      ),
      petriPassed: parseInt(
        metadataElement?.getElementsByTagName("petripassed")[0]?.textContent ||
          "0",
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

  return runs;
}

// Function to parse detailed run data from XML using lightweight regex parsing
function parseRunDetails(
  xmlTextArray: string[],
  runNumber: string,
): RunDetailsData {
  const testFolders = new Map<
    string,
    { hasJsonl: boolean; hasPassed: boolean }
  >();

  // Extract creation time from the first blob (check first string in array)
  let creationTime: Date | null = null;
  if (xmlTextArray.length > 0) {
    try {
      const creationTimeMatch = xmlTextArray[0].match(
        /<Creation-Time>([^<]+)<\/Creation-Time>/,
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
  }

  // Regex to extract Name elements from Blob entries
  // This avoids creating a full DOM tree and just scans the text
  const nameRegex = /<Name>([^<]+)<\/Name>/g;

  // Process each string in the array
  for (const xmlText of xmlTextArray) {
    let match;
    // Reset regex lastIndex for each new string
    nameRegex.lastIndex = 0;

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
 */
export async function fetchRunDetails(
  runNumber: string,
  signal?: AbortSignal,
): Promise<RunDetailsData> {
  try {
    const xmlDataArray: string[] = [];
    let continuationToken: string | null = null;

    // Collect all XML data first in an array to avoid string concatenation overhead
    do {
      // Build URL with continuation token if we have one
      let url = `https://openvmmghtestresults.blob.core.windows.net/results?restype=container&comp=list&showonly=files&prefix=${encodeURIComponent(runNumber + "/")}`;
      if (continuationToken) {
        url += `&marker=${encodeURIComponent(continuationToken)}`;
      }

      const response = await fetch(url, { signal });
      if (!response.ok) {
        throw new Error(
          `Failed to fetch run details: ${response.status} ${response.statusText}`,
        );
      }

      const data = await response.text();
      xmlDataArray.push(data);

      // Check for NextMarker using regex instead of DOMParser (more memory efficient)
      const nextMarkerMatch = data.match(/<NextMarker>([^<]+)<\/NextMarker>/);
      continuationToken = nextMarkerMatch ? nextMarkerMatch[1] : null;
    } while (continuationToken);

    // Parse all collected data at once by scanning through the array
    signal?.throwIfAborted();
    return parseRunDetails(xmlDataArray, runNumber);
  } catch (error) {
    if (!signal?.aborted) console.error(`Error fetching run details`, error);
    throw error;
  }
}

/**
 * Fetch run details for runs filtered by branch.
 * Returns a map of testName -> TestRunInfo[].
 *
 * DEV NOTE: Ideally the abort signal should also be passed to the query client
 * to abort any in-flight fetches, but due to time constraints this is not yet
 * implemented. Currently the tests and test details both use a foreground
 * concurrency of 15 which means that once this function is triggered it will
 * fully fetch AT LEAST that many requests before it can respond to an abort.
 *
 * @param getConcurrency - Optional callback to get current max concurrent requests (defaults to 5)
 */
export async function fetchTestAnalysis(
  branchFilter: string,
  queryClient: QueryClient,
  onProgress?: (fetched: number, total: number) => void,
  getConcurrency?: () => number,
  signal?: AbortSignal,
): Promise<Map<string, TestRunInfo[]>> {
  // Fetch all runs
  const runs = await queryClient.ensureQueryData<RunData[]>({
    queryKey: ["runs"],
    queryFn: () => fetchRunData(queryClient),
    staleTime: 2 * 60 * 1000, // refetch every 2 minutes
    gcTime: Infinity, // never garbage collect
  });

  // Filter runs based on branch selection
  let filteredRuns = runs;
  if (branchFilter !== "all") {
    filteredRuns = runs.filter((run) => run.metadata.ghBranch === branchFilter);
  }

  const totalToFetch = filteredRuns.length;
  let fetchedCount = 0;

  // Set to initial progress
  onProgress?.(fetchedCount, totalToFetch);

  const prefetchRun = async (run: RunData) => {
    const runId = run.name.split("/")[1]; // run.name is "attempts/<runId>_<attempt>", we want the "<runId>_<attempt>" key
    const key = ["runDetails", runId];

    // Check if already aborted
    if (signal?.aborted) {
      throw new DOMException("Aborted", "AbortError");
    }

    // Skip if already cached
    if (queryClient.getQueryData(key)) {
      fetchedCount++;
      if (onProgress) {
        onProgress(fetchedCount, totalToFetch);
      }
      return runId;
    }

    try {
      await queryClient.prefetchQuery({
        queryKey: key,
        queryFn: ({ signal }) => fetchRunDetails(runId, signal),
        staleTime: Infinity, // never goes stale because this data should never change
        gcTime: Infinity, // never garbage collect
      });

      // Increment counter and report progress after each prefetch completes
      fetchedCount++;
      if (onProgress) {
        onProgress(fetchedCount, totalToFetch);
      }

      return runId;
    } catch (e) {
      console.warn(`[fetchTestAnalysis] Prefetch failed for run`, e);
      fetchedCount++;
      if (onProgress) {
        onProgress(fetchedCount, totalToFetch);
      }
      return runId;
    }
  };

  // Process with rolling window - always keep maxConcurrent requests in flight
  let currentIndex = 0;
  const inFlight = new Set<Promise<string>>();

  // Prefetch with controlled parallelism - maintains constant concurrent requests
  // Use dynamic concurrency if provided, otherwise default to 5
  const runIds: string[] = [];

  while (currentIndex < filteredRuns.length || inFlight.size > 0) {
    // Get current concurrency limit (can change dynamically)
    const maxConcurrent = getConcurrency ? getConcurrency() : 5;

    // Fill up to maxConcurrent
    while (
      currentIndex < filteredRuns.length &&
      inFlight.size < maxConcurrent
    ) {
      const promise = prefetchRun(filteredRuns[currentIndex]);
      inFlight.add(promise);
      currentIndex++;

      // Clean up when done and collect result
      promise.then(
        (runId) => {
          inFlight.delete(promise);
          if (runId) runIds.push(runId);
        },
        () => {
          inFlight.delete(promise);
        },
      );
    }

    // Wait for at least one to complete before continuing
    if (inFlight.size > 0) {
      await Promise.race(inFlight);
    }

    // If it was aborted don't do the consolidation step
    if (signal?.aborted) {
      throw new DOMException("Aborted", "AbortError");
    }
  }

  // Build the map from cached data
  const runDetailsMap = new Map<string, RunDetailsData>();
  runIds.forEach((runId) => {
    const runDetails = queryClient.getQueryData<RunDetailsData>([
      "runDetails",
      runId,
    ]);
    if (runDetails) {
      runDetailsMap.set(runId, runDetails);
    }
  });

  // Create mapping of testName -> TestRunInfo[]
  const testMapping = new Map<string, TestRunInfo[]>();

  runDetailsMap.forEach((runDetails) => {
    runDetails.tests.forEach((test) => {
      const testName = test.name;
      const testRunInfo: TestRunInfo = {
        runNumber: runDetails.runNumber,
        status: test.status,
        creationTime: runDetails.creationTime,
      };

      if (!testMapping.has(testName)) {
        testMapping.set(testName, []);
      }
      testMapping.get(testName)!.push(testRunInfo);
    });
  });

  return testMapping;
}

/**
 * Convert test mapping to table data.
 * Transforms a map of test names to their run information into a flat array of TestData.
 */
export function convertToTestData(
  testMapping: Map<string, TestRunInfo[]>,
): TestData[] {
  const data: TestData[] = [];

  testMapping.forEach((runInfos, testName) => {
    const failedCount = runInfos.filter(
      (info) => info.status === "failed",
    ).length;

    // Default to - to avoid skipping entries. Added benefit here is that
    // linking is on test name. So if split was unsuccessful, we won't make a
    // bad link either.
    const split = testName.split("/");
    const architecture: string = split[0]; // Minimum len is 1. This is safe to do.
    const name: string = split.length > 1 ? split[1] : "-";
    const totalCount = runInfos.length;

    data.push({
      architecture,
      name,
      failedCount,
      totalCount,
    });
  });

  return data;
}

/**
 * Convert test mapping to test details data for a specific test.
 * Extracts the run information for a single test from the mapping.
 */
export function convertToTestDetailsData(
  testMapping: Map<string, TestRunInfo[]>,
  testName: string,
): TestRunInfo[] {
  const testRunInfos = testMapping.get(testName);
  return testRunInfos || [];
}
