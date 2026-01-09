// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { ColumnDef } from '@tanstack/react-table';
import { Link } from 'react-router-dom';
import { TestData } from '../data_defs';
import '../styles/common.css';

export const defaultSorting = [
    { id: 'failedCount', desc: true }
];

export const columnWidthMap = {
    architecture: 300,
    failedCount: 50,
    totalCount: 60,
    status: 60
};

// Define the columns for the tests table
export const createColumns = (branchFilter: string): ColumnDef<TestData>[] => {
    return [
        {
            id: 'status',
            header: 'Status',
            enableSorting: true,
            accessorFn: (row) => row.failedCount === 0 ? 'passed' : 'failed',
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
            accessorKey: 'failedCount',
            header: 'Fail',
            enableSorting: true,
            cell: (info) => (
                <div className="common-failed-count">{info.getValue<number>()}</div>
            ),
        },
        {
            accessorKey: 'totalCount',
            header: 'Total',
            enableSorting: true,
            cell: (info) => (
                <div className="common-total-count">{info.getValue<number>()}</div>
            ),
        },
        {
            id: 'architecture',
            accessorKey: 'architecture',
            header: 'Architecture',
            enableSorting: true,
            cell: (info) => (
                <div className="common-architecture-name" title={info.getValue() as string}>
                    {info.getValue() as string}
                </div>
            ),
            sortingFn: (rowA, rowB, columnId) => {
                const a = rowA.getValue(columnId) as string;
                const b = rowB.getValue(columnId) as string;
                return a.localeCompare(b);
            },
        },
        {
            accessorKey: 'name',
            header: 'Test Name',
            enableSorting: true,
            cell: (info) => {
                const name = info.getValue() as string;
                const architecture = info.row.original.architecture;
                const encodedArchitecture = encodeURIComponent(architecture);
                const encodedTestName = encodeURIComponent(name);
                const encodedBranchName = encodeURIComponent(branchFilter);
                return (
                    <Link
                        to={`/tests/${encodedArchitecture}/${encodedTestName}?branchFilter=${encodedBranchName}`}
                        state={{ testData: info.row.original }}
                        className="common-table-link"
                        title={name}
                    >
                        {name}
                    </Link>
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
