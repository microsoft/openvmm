// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import './styles/common.css';
import React, { useState, useEffect, useMemo, useRef } from 'react';
import { SortingState } from '@tanstack/react-table';
import { useQueryClient } from '@tanstack/react-query';
import { fetchTestAnalysis, convertToTestData } from './fetch/fetch_runs_data';
import { CONCURRENCY_BACKGROUND, CONCURRENCY_FOREGROUND, TestData } from './data_defs';
import { Menu } from './menu.tsx';
import { VirtualizedTable } from './virtualized_table.tsx';
import { Link, useSearchParams } from 'react-router-dom';
import { SearchInput } from './search';
import { createColumns, defaultSorting, columnWidthMap } from './table_defs/tests';

export function Tests(): React.JSX.Element {
    const [searchParams, setSearchParams] = useSearchParams();
    const branchFromUrl = searchParams.get('branchFilter') || 'main';
    const [branchFilter, setBranchFilterState] = useState<string>(branchFromUrl);
    const [searchFilter, setSearchFilter] = useState<string>('');
    const [tableData, setTableData] = useState<TestData[]>([]);
    const [fetchedCount, setFetchedCount] = useState<number>(0);
    const [totalToFetch, setTotalToFetch] = useState<number | null>(null);
    const queryClient = useQueryClient();

    // Track component mount state for dynamic concurrency control
    const concurrencyRef = useRef(CONCURRENCY_FOREGROUND);

    // Update concurrency based on mount state
    useEffect(() => {
        concurrencyRef.current = CONCURRENCY_FOREGROUND;
        return () => {
            concurrencyRef.current = CONCURRENCY_BACKGROUND;
        };
    }, []);

    // Sync state with URL on mount and when URL changes
    useEffect(() => {
        setBranchFilterState(branchFromUrl);
    }, [branchFromUrl]);

    // Update both state and URL when branch filter changes
    const setBranchFilter = (branch: string) => {
        setBranchFilterState(branch);
        const newParams = new URLSearchParams(searchParams);
        newParams.set('branchFilter', branch);
        setSearchParams(newParams, { replace: true });
    };

    // Fetch run details for the selected branch
    useEffect(() => {
        setFetchedCount(0);

        // Fetch test analysis (which returns the test mapping)
        fetchTestAnalysis(
            branchFilter,
            queryClient,
            (fetched, total) => {
                setFetchedCount(fetched);
                setTotalToFetch(total);
            },
            () => concurrencyRef.current // Dynamic concurrency
        ).then(testMapping => {
            setTableData(convertToTestData(testMapping));
        }).catch(err => {
            console.error('Error fetching test analysis:', err);
        });
    }, [branchFilter, queryClient]);

    // Get the table definition (columns and default sorting)
    const [sorting, setSorting] = useState<SortingState>(defaultSorting);
    const columns = useMemo(() => createColumns(), []);
    const filteredTableData = useMemo(() => filterTests(tableData, searchFilter), [tableData, searchFilter]);

    return (
        <div className="common-page-display">
            <div className="common-page-header">
                <TestsHeader
                    branchFilter={branchFilter}
                    setBranchFilter={setBranchFilter}
                    searchFilter={searchFilter}
                    setSearchFilter={setSearchFilter}
                    resultCount={filteredTableData.length}
                    fetchedCount={fetchedCount}
                    totalToFetch={totalToFetch}
                />
            </div>
            <VirtualizedTable
                data={filteredTableData}
                columns={columns}
                sorting={sorting}
                columnWidthMap={columnWidthMap}
                onSortingChange={setSorting}
            />
        </div>
    );
}

interface TestsHeaderProps {
    branchFilter: string;
    setBranchFilter: (branch: string) => void;
    searchFilter: string;
    setSearchFilter: (filter: string) => void;
    resultCount: number;
    fetchedCount: number;
    totalToFetch: number | null;
}

export function TestsHeader({
    branchFilter,
    setBranchFilter,
    searchFilter,
    setSearchFilter,
    resultCount,
    fetchedCount,
    totalToFetch,
}: TestsHeaderProps): React.JSX.Element {
    return (
        <>
            <div className="common-header-left">
                <div className="common-header-title">
                    <Menu />
                    <Link to="/tests" className="common-header-path">Tests</Link>
                </div>
                <div className="common-header-filter-buttons">
                    <button
                        className={`common-header-filter-btn ${branchFilter === 'main' ? 'active' : ''}`}
                        onClick={() => setBranchFilter('main')}
                    >
                        main
                    </button>
                </div>
                {totalToFetch === null && (
                    <div className="header-loading-indicator">
                        <div className="header-loading-spinner"></div>
                        <div className="header-loading-text">
                            Fetching runs ...
                        </div>
                    </div>
                )}
                {(fetchedCount !== totalToFetch) && (totalToFetch !== null) && (
                    <div className="header-loading-indicator">
                        <div className="header-loading-spinner"></div>
                        <div className="header-loading-text">
                            Analyzed {fetchedCount}/{totalToFetch}
                        </div>
                    </div>
                )}
            </div>
            <div className="common-header-right">
                <SearchInput value={searchFilter} onChange={setSearchFilter} />
                <span className="common-result-count">
                    {resultCount} tests
                </span>
            </div>
        </>
    );
}

/**
 * filterTests filters the list of tests based on search terms.
 * 
 * - Search string is split into terms (by whitespace), and each test is checked
 *   to see if ALL terms are present.
 * - The searchable fields include: architecture and test name.
 * - The filtering is case-insensitive.
 */
function filterTests(tests: TestData[], searchFilter: string): TestData[] {
    const terms = searchFilter.trim().toLowerCase().split(/\s+/).filter(Boolean);
    if (terms.length === 0) return tests;
    return tests.filter(test => {
        // Search in architecture and name fields
        const status = test.failedCount === 0 ? 'passed' : 'failed';
        const haystack = `${status} ${test.architecture} ${test.name}`.toLowerCase();
        return terms.every(term => haystack.includes(term));
    });
}
