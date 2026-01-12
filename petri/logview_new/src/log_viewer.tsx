// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import React, { useState, useEffect, useRef, useMemo } from 'react';
import { Menu } from './menu';
import { Link, useParams, useLocation, useNavigate } from 'react-router-dom';
import { VirtualizedTable } from './virtualized_table';
import { fetchProcessedLog } from './fetch/fetch_logs_data';
import { useQuery } from '@tanstack/react-query';
import { SortingState } from '@tanstack/react-table';
import './styles/common.css';
import './styles/log_viewer.css';
import { SearchInput } from './search';
import { createColumns, columnWidthMap, defaultSorting } from './table_defs/log_viewer';
import { LogEntry } from './data_defs';
import { InspectOverlay } from './inspect';

interface LogViewerHeaderProps {
    runId: string;
    architecture: string;
    testNameRemainder: string; // portion after architecture
    fullTestName: string; // architecture + '/' + remainder
    searchFilter: string;
    setSearchFilter: (filter: string) => void;
    searchInputRef?: React.RefObject<HTMLInputElement | null>;
    /** Whether the search input should have its global key handlers active */
    searchActive?: boolean;
}

function LogViewerHeader({ runId, architecture, testNameRemainder, fullTestName, searchFilter, setSearchFilter, searchInputRef, searchActive = true }: LogViewerHeaderProps): React.JSX.Element {
    const encodedArchitecture = encodeURIComponent(architecture);
    const encodedRemainder = encodeURIComponent(testNameRemainder);

    return (
        <>
            <div className="common-header-left" style={{ minWidth: 0, flex: 1, display: 'flex' }}>
                <div
                    className="common-header-title"
                    style={{
                        minWidth: 0,
                    }}
                >
                    <Menu />
                    <Link to={`/runs/${runId}`} className="common-header-path">{runId}</Link>
                    <span style={{ flexShrink: 0 }}>/</span>
                    <Link
                        to={`/runs/${runId}/${encodedArchitecture}/${encodedRemainder}`}
                        className="common-header-path-long"
                        title={fullTestName}
                    >
                        {testNameRemainder}
                    </Link>
                    {architecture && (
                        <div
                            className="common-sub-header"
                        >
                            {architecture}
                        </div>
                    )}
                </div>
            </div>
            <div className="runs-header-right-section">
                <SearchInput value={searchFilter} onChange={setSearchFilter} inputRef={searchInputRef} active={searchActive} />
            </div>
        </>
    );
}

export function LogViewer(): React.JSX.Element {
    const location = useLocation();
    const navigate = useNavigate();

    const [searchFilter, setSearchFilter] = useState<string>('');
    const [selectedRow, setSelectedRow] = useState<string | null>(null);
    const [pendingScrollIndex, setPendingScrollIndex] = useState<number | null>(null);
    const [screenshot, setScreenshot] = useState<string | null>(null);
    const [sorting, setSorting] = useState<SortingState>(defaultSorting);
    const searchInputRef = useRef<HTMLInputElement>(null);
    const logContainerRef = useRef<HTMLDivElement>(null);
    // Deep link initialization refs
    const initialLogParamRef = useRef<number | null>(null);
    const initializedFromUrlRef = useRef<boolean>(false);
    // Overlay state: supports parsed inspect tree or raw text view
    const [inspectOverlay, setInspectOverlay] = useState<{ url: string; raw: boolean } | null>(null);

    let { runId, architecture, testName } = useParams();
    runId = runId ? decodeURIComponent(runId) : "";
    architecture = architecture ? decodeURIComponent(architecture) : "";
    testName = testName ? decodeURIComponent(testName) : "";
    const fullTestName = `${architecture}/${testName}`;

    // Fetch the relevant data
    const { data: logEntries } = useQuery({
        queryKey: ["petriLog", runId, architecture, testName],
        queryFn: () => fetchProcessedLog(runId, architecture, testName),
        staleTime: Infinity, // never goes stale
        gcTime: Infinity,
    });

    // Define columns for the virtualized table
    const columns = useMemo(() => createColumns(setScreenshot), []);

    // Conditional AND wildcard search
    const filteredLogs = useMemo(
        () => filterLog(logEntries, searchFilter),
        [logEntries, searchFilter]
    );

    // Handle keyboard shortcuts
    // TODO: This does create a handler every time the user clicks but
    // performance impact is negligible. Fix in the future.
    useEffect(() => {
        const handleKeyDown = createKeyboardHandler(selectedRow, logEntries);
        document.addEventListener('keydown', handleKeyDown);
        return () => document.removeEventListener('keydown', handleKeyDown);
    }, [selectedRow, logEntries]);

    // Capture the initial ?log param once per runId/testName change
    useEffect(() => {
        initializedFromUrlRef.current = false;
        const params = new URLSearchParams(location.search);
        const raw = params.get('log');
        if (raw == null) {
            initialLogParamRef.current = null;
            // Even if absent we consider initialization done (no auto-selection needed)
            initializedFromUrlRef.current = true;
            return;
        }
        const parsed = parseInt(raw, 10);
        if (isNaN(parsed)) {
            initialLogParamRef.current = null;
            initializedFromUrlRef.current = true;
            return;
        }
        initialLogParamRef.current = parsed;
    }, [runId, fullTestName]);

    // Perform one-time selection & scroll after logs load (deep link only)
    useEffect(() => {
        if (initializedFromUrlRef.current) return; // already handled (or none needed)
        const target = initialLogParamRef.current;
        if (target == null) return; // nothing to do
        if (!logEntries || !logEntries.length) return; // wait for entries
        const entryExists = logEntries.some(le => le.index === target);
        if (!entryExists) {
            initializedFromUrlRef.current = true; // finalize even if missing
            return;
        }
        const logId = `log-${target}`;
        setSelectedRow(logId);
        const displayIdx = filteredLogs.findIndex((l: LogEntry) => l.index === target);
        if (displayIdx >= 0) setPendingScrollIndex(displayIdx);
        initializedFromUrlRef.current = true; // prevent future runs
    }, [logEntries, filteredLogs]);

    // Once we've scrolled for the deep link, clear the pending index so subsequent clicks don't re-scroll
    useEffect(() => {
        if (pendingScrollIndex == null) return;
        const id = requestAnimationFrame(() => setPendingScrollIndex(null));
        return () => cancelAnimationFrame(id);
    }, [pendingScrollIndex]);

    // When search filter changes and a row is selected, scroll to that row in
    // the filtered results. Don't do anything when only the selected row
    // changes.
    // Whenever search filter changes, so does the set of filtered rows, so
    // don't to include filtered rows in the dependency array.
    useEffect(() => {
        if (!selectedRow) return;
        const targetIndex = parseInt(selectedRow.replace('log-', ''), 10);
        if (isNaN(targetIndex)) return;
        const displayIdx = filteredLogs.findIndex((l: LogEntry) => l.index === targetIndex);
        if (displayIdx >= 0) {
            setPendingScrollIndex(displayIdx);
        }
    }, [searchFilter]);

    // Intercept clicks on inspect attachment links (parsed + raw) to open overlay
    useEffect(() => {
        const handleClick = (e: MouseEvent) => {
            const target = e.target as HTMLElement | null;
            if (!target) return;
            const parsedAnchor = target.closest('a[data-inspect="true"]') as HTMLAnchorElement | null;
            if (parsedAnchor) {
                e.preventDefault();
                setInspectOverlay({ url: parsedAnchor.href, raw: false });
                return;
            }
            const rawAnchor = target.closest('a[data-inspect-raw="true"]') as HTMLAnchorElement | null;
            if (rawAnchor) {
                e.preventDefault();
                setInspectOverlay({ url: rawAnchor.href, raw: true });
                return;
            }
        };
        document.addEventListener('click', handleClick);
        return () => document.removeEventListener('click', handleClick);
    }, []);

    return (
        <div className="common-page-display">
            <div className="common-page-header">
                <LogViewerHeader
                    runId={runId || 'unknown'}
                    architecture={architecture}
                    testNameRemainder={testName}
                    fullTestName={fullTestName}
                    searchFilter={searchFilter}
                    setSearchFilter={setSearchFilter}
                    searchInputRef={searchInputRef}
                    searchActive={inspectOverlay == null}
                />
            </div>

            <div ref={logContainerRef} style={{ fontFamily: 'monospace', fontSize: '14px', position: 'relative' }}>
                <VirtualizedTable<LogEntry>
                    data={filteredLogs}
                    columns={columns}
                    sorting={sorting}
                    onSortingChange={setSorting}
                    columnWidthMap={columnWidthMap}
                    estimatedRowHeight={50}
                    getRowClassName={(row) => {
                        const logId = `log-${row.original.index}`;
                        const isSelected = selectedRow === logId;
                        const severityClass = `severity-${row.original.severity}`;
                        return `${severityClass} ${isSelected ? 'selected' : ''}`;
                    }}
                    overscan={100}
                    onRowClick={(row, event) => {
                        const logId = `log-${row.original.index}`;
                        handleRowClick(
                            row.original.index,
                            logId,
                            event,
                            selectedRow,
                            setSelectedRow,
                            location,
                            navigate
                        );
                    }}
                    scrollToIndex={pendingScrollIndex}
                />
            </div>

            {/* Image Content */}
            {screenshot && (
                <div
                    className="logviewer-image-div"
                    onClick={() => setScreenshot(null)}
                >
                        <img
                            src={screenshot}
                            alt="screenshot"
                            className="logviewer-image-content"
                        />
                </div>
            )}

            {/* Inspect Overlay */}
            {inspectOverlay && (
                <InspectOverlay
                    fileUrl={inspectOverlay.url}
                    rawMode={inspectOverlay.raw}
                    onClose={() => setInspectOverlay(null)}
                />
            )}
        </div>
    );
}

/**
 * Handles row click events in the log viewer.
 * 
 * This function manages row selection with the following behaviors:
 * - Ignores clicks on links within the row
 * - Detects and ignores text selection (dragging) to allow users to copy text
 * - Toggles row selection on/off when clicking the same row
 * - Updates URL query parameters to reflect the selected log entry
 * - Maintains selection state when user is selecting text within a row
 * 
 * @param originalIndex - The original index of the log entry
 * @param logId - The ID of the log row (e.g., "log-123")
 * @param event - The React mouse event from the click
 * @param selectedRow - The currently selected row ID (or null if none selected)
 * @param setSelectedRow - State setter function to update the selected row
 * @param location - React Router location object for URL manipulation
 * @param navigate - React Router navigate function for URL updates
 */
function handleRowClick(
    originalIndex: number,
    logId: string,
    event: React.MouseEvent,
    selectedRow: string | null,
    setSelectedRow: (row: string | null) => void,
    location: ReturnType<typeof useLocation>,
    navigate: ReturnType<typeof useNavigate>
) {
    if ((event.target as HTMLElement).closest('a')) return; // ignore clicks on links

    // Detect if user is performing a text selection inside this row. If so,
    // Don't do anything.
    const sel = window.getSelection();
    const isSelectingText = !!sel && !sel.isCollapsed && sel.toString().trim().length > 0;
    const currentTarget = event.currentTarget as HTMLElement | null;
    let selectionInsideRow = false;
    if (isSelectingText && currentTarget && sel && sel.rangeCount > 0) {
        const range = sel.getRangeAt(0);
        const common = range.commonAncestorContainer;
        selectionInsideRow = currentTarget.contains(common.nodeType === 1 ? common as Node : common.parentElement as Node);
        return;
    }

    const params = new URLSearchParams(location.search);

    // If already selected and the user is dragging/selecting text inside the row, keep selection & ensure URL param is present.
    if (selectedRow === logId && selectionInsideRow) {
        if (!params.get('log')) {
            params.set('log', originalIndex.toString());
            navigate(`${location.pathname}?${params.toString()}`, { replace: true });
        }
        return; // do not toggle off
    }

    if (selectedRow === logId) {
        // Plain click on an already selected row (no text selection) -> toggle off
        setSelectedRow(null);
        params.delete('log');
        navigate(params.toString() ? `${location.pathname}?${params.toString()}` : location.pathname, { replace: true });
        return;
    }

    // Selecting a new row
    setSelectedRow(logId);
    params.set('log', originalIndex.toString());
    navigate(`${location.pathname}?${params.toString()}`, { replace: true });
}

/**
 * Creates a keyboard event handler for log viewer shortcuts.
 * 
 * Handles two copy shortcuts:
 * - Ctrl+C (or Cmd+C): Copy plain text representation of selected log line
 * - Ctrl+Shift+C (or Cmd+Shift+C): Copy deep link to selected log line
 * 
 * @param selectedRow - The ID of the currently selected row (e.g., "log-123")
 * @param logEntries - The array of log entries
 * @returns Keyboard event handler function
 */
function createKeyboardHandler(selectedRow: string | null, logEntries: LogEntry[] | undefined) {
    return (e: KeyboardEvent) => {
        // Custom copy handlers
        const isCopyCombo = (e.key === 'c' || e.key === 'C') && (e.metaKey || e.ctrlKey);
        if (!isCopyCombo) return;

        // Don't override when typing in an input/textarea or there is an actual text selection
        const active = document.activeElement as HTMLElement | null;
        if (active && (active.tagName === 'INPUT' || active.tagName === 'TEXTAREA' || active.isContentEditable)) return;
        const selection = window.getSelection();
        if (selection && !selection.isCollapsed && selection.toString().trim().length > 0) return;

        // Need a selected log row
        if (!selectedRow) return;

        const idx = parseInt(selectedRow.replace('log-', ''), 10);
        if (isNaN(idx)) return;
        const entry = logEntries?.find(le => le.index === idx);
        if (!entry) return;

        if (e.shiftKey) {
            // Ctrl+Shift+C (or Cmd+Shift+C) => copy deep link to this log line (hash-based routing aware)
            e.preventDefault();
            const { origin, pathname, hash } = window.location;
            // HashRouter format: <origin><pathname>#/route/segments?query
            const hashParts = hash.split('?');
            const hashRoute = hashParts[0] || '#/';
            const hashQuery = hashParts[1] ? new URLSearchParams(hashParts[1]) : new URLSearchParams();
            hashQuery.set('log', String(entry.index));
            const deepLink = `${origin}${pathname}${hashRoute}?${hashQuery.toString()}`;
            navigator.clipboard?.writeText(deepLink).catch(() => {
                try {
                    const ta = document.createElement('textarea');
                    ta.value = deepLink; ta.style.position = 'fixed'; ta.style.opacity = '0';
                    document.body.appendChild(ta); ta.select(); document.execCommand('copy'); document.body.removeChild(ta);
                } catch { /* no-op */ }
            });
            return;
        }

        // Plain Ctrl+C => copy plain text representation of the selected log line
        e.preventDefault();
        
        // Decode HTML entities in the message text
        const decodeHtml = (html: string): string => {
            const txt = document.createElement('textarea');
            txt.innerHTML = html;
            return txt.value;
        };
        
        let textBlock = `timestamp: ${entry.timestamp}\n`;
        textBlock += `relative: ${entry.relative}\n`;
        textBlock += `severity: ${entry.severity}\n`;
        textBlock += `source: ${decodeHtml(entry.source)}\n`;
        textBlock += `message: ${decodeHtml(entry.logMessage.message.trim())}`;
        if (entry.screenshot) {
            textBlock += `\nscreenshot: ${entry.screenshot}`;
        }
        
        navigator.clipboard?.writeText(textBlock).catch(() => {
            try {
                const ta = document.createElement('textarea');
                ta.value = textBlock; ta.style.position = 'fixed'; ta.style.opacity = '0';
                document.body.appendChild(ta); ta.select(); document.execCommand('copy'); document.body.removeChild(ta);
            } catch { /* no-op */ }
        });
    };
}

/**
 * Filters log entries based on a search query.
 * 
 * The query is tokenized by whitespace, with support for quoted phrases.
 * Multiple tokens act as AND filters - all tokens must match for the log to pass.
 * 
 * Special field-specific search syntax:
 * - `severity:INFO` - searches only in the severity field
 * - `source:vm` - searches only in the source field
 * - `message:error` - searches only in the message text field
 * 
 * Tokens without a prefix search across all fields (source, severity, and message).
 * 
 * Examples:
 * - `severity:ERROR source:disk` - finds ERROR logs from disk source
 * - `"boot failed" severity:WARN` - finds WARN logs containing the exact phrase "boot failed"
 * - `timeout retry` - finds logs containing both "timeout" and "retry" in any field
 * 
 * @param logs - The array of log entries to filter (can be undefined)
 * @param query - The search query string
 * @returns Filtered array of log entries
 */
function filterLog(logs: LogEntry[] | undefined, query: string): LogEntry[] {
    if (!logs) return [];
    if (!query.trim()) return logs;

    // Tokenize the search query
    let normalizedQuery = query;
    const quoteCount = (query.match(/"/g) || []).length;
    if (quoteCount % 2 !== 0) {
        normalizedQuery += '"';
    }
    const regex = /"([^"]+)"|(\S+)/g;
    const tokens: string[] = [];
    let match;
    while ((match = regex.exec(normalizedQuery))) {
        tokens.push(match[1] || match[2]);
    }

    // Filter logs - check if each log matches all tokens
    return logs.filter(log => {
        return tokens.every(token => {
            const [prefix, ...rest] = token.split(':');
            const term = rest.join(':').toLowerCase();
            const columnSearching = token.includes(':');

            if (columnSearching && prefix === 'source') {
                return log.source.toLowerCase().includes(term);
            } else if (columnSearching && prefix === 'severity') {
                return log.severity.toLowerCase().includes(term);
            } else if (columnSearching && prefix === 'message') {
                return log.logMessage.message.toLowerCase().includes(term)
                    || log.logMessage.link_string.toLowerCase().includes(term);
            } else {
                return (
                    log.source.toLowerCase().includes(token.toLowerCase()) ||
                    log.severity.toLowerCase().includes(token.toLowerCase()) ||
                    log.logMessage.message.toLowerCase().includes(token.toLowerCase()) ||
                    log.logMessage.link_string.toLowerCase().includes(token.toLowerCase())
                );
            }
        });
    });
}
