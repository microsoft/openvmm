// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import "./styles/common.css";
import "./styles/runs.css";
import React, { useState, useMemo, useEffect } from "react";
import { SortingState } from "@tanstack/react-table";
import { useQuery } from "@tanstack/react-query";
import { RunData } from "./data_defs";
import { fetchRunData } from "./fetch/fetch_runs_data.tsx";
import { Menu } from "./menu";
import { VirtualizedTable } from "./virtualized_table.tsx";
import { Link, useSearchParams } from "react-router-dom";
import { SearchInput } from "./search";
import { run_filters } from "./branch_quick_filters";
import {
  createColumns,
  defaultSorting,
  columnWidthMap,
} from "./table_defs/runs";

export function Runs(): React.JSX.Element {
  const [searchParams, setSearchParams] = useSearchParams();
  const branchFromUrl = searchParams.get("branchFilter") || "all";
  const [branchFilter, setBranchFilterState] = useState<string>(branchFromUrl);
  const [searchFilter, setSearchFilter] = useState<string>("");

  // Sync state with URL on mount and when URL changes
  useEffect(() => {
    setBranchFilterState(branchFromUrl);
  }, [branchFromUrl]);

  // Update both state and URL when branch filter changes
  const setBranchFilter = (branch: string) => {
    setBranchFilterState(branch);
    const newParams = new URLSearchParams(searchParams);
    newParams.set("branchFilter", branch);
    setSearchParams(newParams, { replace: true });
  };

  // Fetch the relevant data
  const { data: runs = [], isSuccess } = useQuery({
    queryKey: ["runs"],
    queryFn: (context) => fetchRunData(context.client),
    staleTime: 2 * 60 * 1000, // refetch every 2 minutes
    gcTime: Infinity, // never garbage collect
    refetchInterval: 2 * 60 * 1000, // automatically refetch every 2 minutes
  });

  // Check if the query succeeded but returned no data (not due to filtering, not during loading)
  const hasNoData = isSuccess && runs.length === 0;

  // Get the table definition (columns and default sorting)
  const [sorting, setSorting] = useState<SortingState>(defaultSorting);
  const columns = useMemo(() => createColumns(), []);
  const filteredRuns = useMemo(
    () => filterRuns(runs, branchFilter, searchFilter),
    [runs, branchFilter, searchFilter]
  );

  return (
    <div className="common-page-display">
      <div className="common-page-header">
        <RunsHeader
          branchFilter={branchFilter}
          setBranchFilter={setBranchFilter}
          searchFilter={searchFilter}
          setSearchFilter={setSearchFilter}
          resultCount={filteredRuns.length}
          loadingSuccess={isSuccess}
        />
      </div>
      {hasNoData ? (
        <div className="common-no-data">Table contains no data.</div>
      ) : (
        <VirtualizedTable
          data={filteredRuns}
          columns={columns}
          sorting={sorting}
          onSortingChange={setSorting}
          columnWidthMap={columnWidthMap}
        />
      )}
    </div>
  );
}

interface RunsHeaderProps {
  branchFilter: string;
  setBranchFilter: (branch: string) => void;
  searchFilter: string;
  setSearchFilter: (filter: string) => void;
  resultCount: number;
  loadingSuccess: boolean;
}

export function RunsHeader({
  branchFilter,
  setBranchFilter,
  searchFilter,
  setSearchFilter,
  resultCount,
  loadingSuccess,
}: RunsHeaderProps): React.JSX.Element {
  return (
    <>
      <div className="common-header-left">
        <div className="common-header-title">
          <Menu />
          <Link to="/runs" className="common-header-path">
            Runs
          </Link>
        </div>
        <div className="common-header-filter-buttons">
          {run_filters.map((branch) => (
            <button
              key={branch}
              className={`common-header-filter-btn ${branchFilter === branch ? "active" : ""}`}
              onClick={() => setBranchFilter(branch)}
            >
              {branch}
            </button>
          ))}
        </div>
        {!loadingSuccess && (
          <div className="header-loading-indicator">
              <div className="header-loading-spinner"></div>
              <div className="header-loading-text">
                  Fetching runs ...
              </div>
          </div>
        )}
      </div>
      <div className="common-header-right">
        <SearchInput value={searchFilter} onChange={setSearchFilter} />
        <span className="common-result-count">{resultCount} runs</span>
      </div>
    </>
  );
}

/**
 * filterRuns filters the list of runs based on the selected branch and search terms.
 *
 * - Branch filtering is applied first: if 'all' is selected, all runs are included; otherwise, only runs matching the selected branch are kept.
 * - Search string is split into terms (by whitespace), and each run is checked
 *   to see if ALL terms are present.
 * - The searchable fields include: run name, status (passed/failed), branch name, PR number, and PR title.
 * - The filtering is case-insensitive.
 */
function filterRuns(
  runs: RunData[],
  branchFilter: string,
  searchFilter: string
): RunData[] {
  let branchFiltered =
    branchFilter === "all"
      ? runs
      : runs.filter((run) => run.metadata.ghBranch === branchFilter);
  const terms = searchFilter.trim().toLowerCase().split(/\s+/).filter(Boolean);
  if (terms.length === 0) return branchFiltered;
  return branchFiltered.filter((run) => {
    // Search in run name, status, branch, PR, and PR title
    const status = run.metadata.petriFailed === 0 ? "passed" : "failed";
    const pr = run.metadata.ghPr
      ? `${run.metadata.ghPr} ${run.metadata.prTitle || ""}`
      : "";
    const haystack =
      `${run.name} ${status} ${run.metadata.ghBranch || ""} ${pr}`.toLowerCase();
    return terms.every((term) => haystack.includes(term));
  });
}
