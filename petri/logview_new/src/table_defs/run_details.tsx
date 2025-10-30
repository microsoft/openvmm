// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { ColumnDef } from "@tanstack/react-table";
import { Link } from "react-router-dom";
import { TestResult } from "../data_defs";

export const defaultSorting = [
  { id: "status", desc: false }, // Sort by status ascending, failed tests first
];

export const columnWidthMap = {
  architecture: 320,
  status: 60,
};

// Define columns for the test results table
export const createColumns = (runId: string): ColumnDef<TestResult>[] => [
  {
    accessorKey: "status",
    header: "Status",
    enableSorting: true,
    cell: (info) => {
      const status = info.getValue<string>();
      return (
        <div className="common-status-cell">
          <span
            className={
              status === "passed" ? "common-status-pass" : "common-status-fail"
            }
          ></span>
        </div>
      );
    },
  },
  {
    id: "architecture",
    header: "Architecture",
    accessorFn: (row) => {
      const parts = row.name.split("/");
      return parts.length > 1 ? parts[0] : "Other";
    },
    cell: (info) => (
      <div className="common-architecture-name">{info.getValue() as string}</div>
    ),
    enableSorting: true,
  },
  {
    id: "testName",
    header: "Test Name",
    accessorFn: (row) => {
      const parts = row.name.split("/");
      return parts.length > 1 ? parts.slice(1).join("/") : row.name;
    },
    cell: (info) => {
      const testName = info.getValue() as string; // portion after first '/'
      const fullTestName = info.row.original.name; // architecture/testName...
      const [architecturePart, ...restParts] = fullTestName.split("/");
      const encodedArchitecture = encodeURIComponent(architecturePart);
      const encodedRemainder = encodeURIComponent(restParts.join("/"));
      return (
        <Link
          to={`/runs/${runId}/${encodedArchitecture}/${encodedRemainder}`}
          state={{ testResult: info.row.original }}
          className="common-table-link"
          title={`${fullTestName}`}
        >
          {testName}
        </Link>
      );
    },
    enableSorting: true,
  },
];
