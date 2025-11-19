// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import "./styles/common.css";
import React, { useState, useMemo } from "react";
import { SortingState } from "@tanstack/react-table";
import { TestResult } from "./data_defs";
import { Menu } from "./menu";
import { VirtualizedTable } from "./virtualized_table";
import { Link, useParams } from "react-router-dom";
import { useQuery } from "@tanstack/react-query";
import { fetchRunDetails } from "./fetch/fetch_runs_data";
import { SearchInput } from "./search";
import {
  createColumns,
  defaultSorting,
  columnWidthMap,
} from "./table_defs/run_details";

export function RunDetails(): React.JSX.Element {
  const [searchFilter, setSearchFilter] = useState<string>("");
  const [sorting, setSorting] = useState<SortingState>(defaultSorting);
  let { runId } = useParams();
  runId = runId ? decodeURIComponent(runId) : "";

  // Fetch the relevant data
  const { data: runDetails, isSuccess } = useQuery({
    queryKey: ["runDetails", runId],
    queryFn: (context) => fetchRunDetails(runId, context.client),
    staleTime: Infinity, // never goes stale
    gcTime: Infinity,
  });

  // Define columns for the test results table
  const columns = useMemo(() => createColumns(runId), [runId]);

  // Conditional AND wildcard search
  const filteredTests = useMemo(
    () => filterTests(runDetails?.tests, searchFilter),
    [runDetails?.tests, searchFilter]
  );

  // Check if the original data has no tests (not due to filtering)
  const hasNoData = runDetails?.tests && runDetails.tests.length === 0;

  return (
    <div className="common-page-display">
      <div className="common-page-header">
        <RunDetailsHeader
          resultCount={filteredTests.length}
          runId={runId}
          searchFilter={searchFilter}
          setSearchFilter={setSearchFilter}
          loadingSuccess={isSuccess}
        />
      </div>
      {hasNoData ? (
        <div className="common-no-data">Table contains no data.</div>
      ) : (
        <VirtualizedTable<TestResult>
          data={filteredTests}
          columns={columns}
          sorting={sorting}
          onSortingChange={setSorting}
          columnWidthMap={columnWidthMap}
          estimatedRowHeight={44}
          getRowClassName={(row) =>
            row.original.status === "failed" ? "failed-row" : "passed-row"
          }
        />
      )}
    </div>
  );
}

interface RunDetailsHeaderProps {
  resultCount: number;
  runId: string;
  searchFilter: string;
  setSearchFilter: (filter: string) => void;
  loadingSuccess: boolean;
}

function RunDetailsHeader({
  resultCount,
  runId,
  searchFilter,
  setSearchFilter,
  loadingSuccess,
}: RunDetailsHeaderProps): React.JSX.Element {
  return (
    <>
      <div className="common-header-left">
        <div className="common-header-title">
          <Menu />
          <Link to="/runs" className="common-header-path">
            Runs
          </Link>
          <span>/</span>
          <Link to={`/runs/${runId}`} className="common-header-path">
            {runId}
          </Link>
        </div>
        {!loadingSuccess && (
        <div className="header-loading-indicator">
            <div className="header-loading-spinner"></div>
            <div className="header-loading-text">
                Fetching run details ...
            </div>
        </div>
        )}
      </div>
      <div className="common-header-right">
        <SearchInput value={searchFilter} onChange={setSearchFilter} />
        <span className="common-result-count">{resultCount} tests</span>
      </div>
    </>
  );
}

/**
 * filterTests filters the list of tests based on search terms.
 *
 * - Search string is split into terms (by whitespace), and each test is checked
 *   to see if ALL terms are present.
 * - The searchable fields include: test name and status.
 * - The filtering is case-insensitive.
 */
function filterTests(
  tests: TestResult[] | undefined,
  searchFilter: string
): TestResult[] {
  if (!tests) return [];
  const terms = searchFilter.trim().toLowerCase().split(/\s+/).filter(Boolean);
  if (terms.length === 0) return tests;
  return tests.filter((test) => {
    // Search in name and status fields
    const haystack = `${test.name} ${test.status}`.toLowerCase();
    return terms.every((term) => haystack.includes(term));
  });
}
