// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { ColumnDef } from '@tanstack/react-table';
import { TestRunInfo, parseRunKey } from '../data_defs';
import { Link } from 'react-router-dom';
import '../styles/common.css';

export const defaultSorting = [
    { id: 'creationTime', desc: true }
];

export const columnWidthMap = {
    creationTime: 210,
    status: 60,
    runNumber: 120,
    attempt: 75
};

// Define the columns for the test details table
export const createColumns = (testName?: string): ColumnDef<TestRunInfo>[] => {
    return [
        {
            id: 'status',
            header: 'Status',
            accessorKey: 'status',
            enableSorting: true,
            cell: (info) => {
                const status = info.getValue<string>();
                return (
                    <div className="common-status-cell">
                        <div className={status === 'passed' ? 'common-status-pass' : 'common-status-fail'}>
                        </div>
                    </div>
                );
            },
        },
        {
            id: 'creationTime',
            accessorKey: 'creationTime',
            header: 'Created',
            enableSorting: true,
            cell: (info) => {
                const date = info.getValue() as Date | undefined;
                return (
                    <div className="created-date" title={date?.toLocaleString()}>
                        {date?.toLocaleString()}
                    </div>
                );
            },
            sortingFn: (rowA, rowB, columnId) => {
                const a = rowA.getValue(columnId) as Date | undefined;
                const b = rowB.getValue(columnId) as Date | undefined;
                if (!a && !b) return 0;
                if (!a) return 1;
                if (!b) return -1;
                return a.getTime() - b.getTime();
            },
        },
        {
            id: 'runNumber',
            accessorKey: 'runNumber',
            header: 'Run Number',
            enableSorting: true,
            cell: (info) => {
                const runNumber = info.getValue() as string;
                const { runId } = parseRunKey(runNumber);
                const searchParams = testName ? `?search=${encodeURIComponent(testName)}` : '';
                return (
                    <Link to={`/runs/${runNumber}${searchParams}`} className="common-table-link" title={runId}>
                        {runId}
                    </Link>
                );
            },
            sortingFn: (rowA, rowB, columnId) => {
                const a = rowA.getValue(columnId) as string;
                const b = rowB.getValue(columnId) as string;
                return a.localeCompare(b);
            },
        },
        {
            id: 'attempt',
            accessorKey: 'runNumber',
            header: 'Attempt',
            enableSorting: true,
            cell: (info) => {
                const { attempt } = parseRunKey(info.getValue() as string);
                return (
                    <div className="common-total-count">
                        {attempt || '-'}
                    </div>
                );
            },
            sortingFn: (rowA, rowB, columnId) => {
                const a = parseInt(parseRunKey(rowA.getValue(columnId) as string).attempt) || 0;
                const b = parseInt(parseRunKey(rowB.getValue(columnId) as string).attempt) || 0;
                return a - b;
            },
        },
        {
            id: 'ghRun',
            accessorKey: 'runNumber',
            header: 'GH Run',
            enableSorting: true,
            cell: (info) => {
                const runNumber = info.getValue() as string;
                const { runId, attempt } = parseRunKey(runNumber);
                const href = attempt
                    ? `https://github.com/microsoft/openvmm/actions/runs/${runId}/attempts/${attempt}`
                    : `https://github.com/microsoft/openvmm/actions/runs/${runId}`;
                return (
                    <a
                        href={href}
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
