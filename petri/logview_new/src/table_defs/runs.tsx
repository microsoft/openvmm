// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { ColumnDef } from "@tanstack/react-table";
import { RunData } from "../data_defs";
import "../styles/runs.css";

export const defaultSorting = [{ id: "creationTime", desc: true }];

export const columnWidthMap = {
  name: 115,
  creationTime: 210,
  status: 60,
  failed: 50,
  total: 60,
  ghRun: 115,
};

// Define the columns for the runs table
export const createColumns = (
  onRunClick: (runId: string) => void
): ColumnDef<RunData>[] => {
  return [
    {
      id: "status",
      header: "Status",
      enableSorting: true,
      accessorFn: (row) =>
        row.metadata.petriFailed === 0 ? "passed" : "failed",
      cell: (info) => {
        const status = info.getValue<string>();
        return (
          <div className="common-status-cell">
            <div
              className={
                status === "passed"
                  ? "common-status-pass"
                  : "common-status-fail"
              }
            ></div>
          </div>
        );
      },
    },
    {
      id: "failed",
      accessorKey: "metadata.petriFailed",
      header: "Fail",
      enableSorting: true,
      cell: (info) => (
        <div className="common-failed-count">{info.getValue<number>()}</div>
      ),
    },
    {
      id: "total",
      header: "Total",
      enableSorting: true,
      accessorFn: (row) => row.metadata.petriPassed + row.metadata.petriFailed,
      cell: (info) => (
        <div className="common-total-count">{info.getValue<number>()}</div>
      ),
    },
    {
      accessorKey: "name",
      header: "Run",
      enableSorting: true,
      cell: (info) => {
        const runId = info.getValue<string>().replace("runs/", "");
        return (
          <a
            href="#"
            onClick={(e) => {
              e.preventDefault();
              onRunClick(runId);
            }}
            className="common-table-link"
          >
            {runId}
          </a>
        );
      },
      sortingFn: (rowA, rowB, columnId) => {
        const a = rowA.getValue(columnId) as string;
        const b = rowB.getValue(columnId) as string;
        return a.localeCompare(b);
      },
    },
    {
      accessorKey: "creationTime",
      header: "Created",
      enableSorting: true,
      cell: (info) => (
        <div className="created-date">
          {info.getValue<Date>().toLocaleString()}
        </div>
      ),
      sortingFn: (rowA, rowB, columnId) => {
        const a = rowA.getValue(columnId) as Date;
        const b = rowB.getValue(columnId) as Date;
        return a.getTime() - b.getTime();
      },
    },
    {
      accessorKey: "metadata.ghBranch",
      header: "Branch",
      enableSorting: true,
      cell: (info) => {
        const branch = info.getValue<string>() || "";
        return (
          <div
            className="branch-name"
            title={branch}
            style={{
              overflow: "hidden",
              textOverflow: "ellipsis",
              whiteSpace: "nowrap",
              lineHeight: "1.25rem",
            }}
          >
            {branch}
          </div>
        );
      },
    },
    {
      accessorKey: "metadata.ghPr",
      header: "PR",
      enableSorting: true,
      accessorFn: (row) => {
        const pr = row.metadata.ghPr;
        const prTitle = row.metadata.prTitle;
        // Combine PR number and title for searching
        return pr ? `${pr} ${prTitle || ""}`.trim() : "";
      },
      cell: (info) => {
        const row = info.row.original;
        const pr = row.metadata.ghPr;
        const prTitle = row.metadata.prTitle;
        const fullText = pr ? `#${pr}${prTitle ? ` ${prTitle}` : ""}` : "";
        return pr ? (
          <div className="pr-cell">
            <a
              href={`https://github.com/microsoft/openvmm/pull/${pr}`}
              target="_blank"
              rel="noopener noreferrer"
              className="pr-link"
              title={prTitle ? `#${pr} ${prTitle}` : `PR #${pr}`}
            >
              {fullText}
            </a>
          </div>
        ) : (
          <div className="no-pr">-</div>
        );
      },
      sortingFn: (rowA, rowB) => {
        const a = rowA.original.metadata.ghPr;
        const b = rowB.original.metadata.ghPr;
        if (!a && !b) return 0;
        if (!a) return 1;
        if (!b) return -1;
        return parseInt(a) - parseInt(b);
      },
    },
    {
      id: "ghRun", // distinct id to avoid clashing with first 'name' accessor
      accessorKey: "name",
      header: "GH Run",
      enableSorting: true,
      cell: (info) => {
        const runId = info.getValue<string>().replace("runs/", "");
        return (
          <a
            href={`https://github.com/microsoft/openvmm/actions/runs/${runId}`}
            target="_blank"
            rel="noopener noreferrer"
            className="common-table-link"
          >
            {runId}
          </a>
        );
      },
      sortingFn: (rowA, rowB, columnId) => {
        const a = rowA.getValue(columnId) as string;
        const b = rowB.getValue(columnId) as string;
        return a.localeCompare(b);
      },
    },
  ];
};
