// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import React, { useEffect, useRef, useState } from 'react';
import './styles/inspect.css';
import { SearchInput } from './search';
import { type InspectObject, type InspectNode, type InspectPrimitive } from './data_defs';
import { getInspectFile } from './fetch/fetch_inspect_data';

/**
 * Port of old inspect.html functionality into a React overlay component.
 * Follows the original implementation closely, using direct DOM manipulation
 * for expand/collapse to maintain performance.
 */

interface InspectOverlayProps {
    fileUrl: string;           // Absolute URL (already resolved)
    onClose: () => void;       // Close callback
    rawMode?: boolean;         // If true, show raw text (no parsing) with highlight support
}

export const InspectOverlay: React.FC<InspectOverlayProps> = ({ fileUrl, onClose, rawMode = false }) => {
    const [error, setError] = useState<string | null>(null);
    const [data, setData] = useState<InspectObject | null>(null);
    const [filter, setFilter] = useState('');
    const [allExpanded, setAllExpanded] = useState(false);
    const contentsRef = useRef<HTMLDivElement>(null);
    const selectedPathRef = useRef<string>('');
    const allToggleButtonsRef = useRef<HTMLElement[]>([]);
    const [rawText, setRawText] = useState<string>('');
    // Track and preserve expansion state across searching
    const preSearchExpandedRef = useRef<Set<string>>(new Set());
    const previousFilterRef = useRef<string>('');
    const clearedSearchRef = useRef<boolean>(false);

    const fileName = (() => {
        try { const u = new URL(fileUrl); return u.pathname.split('/').filter(Boolean).slice(-1)[0] || fileUrl; } catch { return fileUrl; }
    })();

    // Fetch content once on mount (different paths for raw vs parsed mode)
    useEffect(() => {
        setError(null);
        if (rawMode) {
            fetch(fileUrl)
                .then(r => {
                    if (!r.ok) throw new Error(`Failed to fetch "${fileUrl}": ${r.status} ${r.statusText}. Please check the file path or network connection.`);
                    return r.text();
                })
                .then(t => setRawText(t))
                .catch((e: any) => setError(e.message || String(e)));
        } else {
            getInspectFile(fileUrl)
                .then(parsed => {
                    setData(parsed);
                })
                .catch((e: any) => {
                    setError(e.message || String(e));
                });
        }
    }, []); // Only run on mount

    // Render the tree using direct DOM manipulation (like original). This will
    // also handle filtering and such.
    useEffect(() => {
        // Detect transitions into and out of searching so we can snapshot/restore expansion state.
        const trimmed = filter.trim();
        const prevTrimmed = previousFilterRef.current.trim();

        // Transition: starting a search (empty -> non-empty)
        if (prevTrimmed === '' && trimmed !== '') {
            // Snapshot currently expanded paths before applying filter auto-expansion.
            preSearchExpandedRef.current.clear();
            // The toggle refs are populated from the last render.
            (allToggleButtonsRef.current as any[]).forEach(tc => {
                if (tc.isExpanded && tc.isExpanded() && tc.path) {
                    preSearchExpandedRef.current.add(tc.path as string);
                }
            });
        }
        // Transition: clearing a search (non-empty -> empty). We mark it and restore
        // after the tree is rebuilt in the main render effect below.
        else if (prevTrimmed !== '' && trimmed === '') {
            clearedSearchRef.current = true;
        }

        previousFilterRef.current = filter;
    }, [filter]);

    // Render / re-render tree (or raw lines) when inputs change.
    // This effect also restores expansion state after a search is cleared.
    useEffect(() => {
        if (rawMode) {
            renderRawTextContent(contentsRef, rawText, filter);
        } else {
            // Parsed mode
            renderParsedModeContent(
                data,
                error,
                filter,
                contentsRef,
                selectedPathRef,
                allToggleButtonsRef,
                preSearchExpandedRef,
                clearedSearchRef,
                setAllExpanded
            );
        }
    }, [data, error, filter, rawMode, rawText]);

    // Handle tree node clicks for selection
    useEffect(() => {
        if (rawMode) return; // No selection handling in raw mode
        if (!contentsRef.current) return;
        const handleClick = createTreeNodeClickHandler(contentsRef, selectedPathRef);
        contentsRef.current.addEventListener('click', handleClick);
        return () => {
            contentsRef.current?.removeEventListener('click', handleClick);
        };
    }, [error, rawMode]);

    // Keyboard shortcuts:
    //  '+' or '='      : expand all descendants of selected node
    //  '-'             : collapse all descendants of selected node
    //  ArrowRight      : expand selected node (only its own subtree, not deeper descendants unless already expanded)
    //  ArrowLeft       : collapse selected node
    //  ArrowUp/Down    : move selection to previous/next visible tree row
    useEffect(() => {
        if (rawMode) return; // Only meaningful in parsed mode
        const handler = (e: KeyboardEvent) => {
            // Handle copy of selected subtree: Ctrl+C / Cmd+C (unless user has a text selection)
            if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'c') {
                const selection = window.getSelection();
                if (selection && selection.toString().length > 0) {
                    // User has highlighted text; allow native copy behavior.
                    return; // Do not prevent default
                }
                const selPath = selectedPathRef.current;
                if (selPath && data) {
                    const node = getNodeByPath(data, selPath);
                    if (node) {
                        // Include the selected node name as a root wrapper (e.g. key { ... })
                        const rootKey = selPath.split('.').slice(-1)[0];
                        const text = formatNodeWithRootName(rootKey, node);
                        // Attempt clipboard write
                        const doCopy = async () => {
                            try {
                                await navigator.clipboard.writeText(text);
                            } catch {
                                // Fallback: temporary textarea
                                const ta = document.createElement('textarea');
                                ta.value = text;
                                ta.style.position = 'fixed';
                                ta.style.left = '-1000px';
                                document.body.appendChild(ta);
                                ta.select();
                                try { document.execCommand('copy'); } catch {}
                                document.body.removeChild(ta);
                            }
                        };
                        doCopy();
                        e.preventDefault();
                        e.stopPropagation();
                        return; // Copy handled
                    }
                }
            }
            if (e.altKey || e.ctrlKey || e.metaKey) return; // ignore other shortcuts with modifiers
            const targetEl = e.target as HTMLElement | null;
            if (targetEl) {
                const tag = targetEl.tagName;
                if (tag === 'INPUT' || tag === 'TEXTAREA' || targetEl.isContentEditable) return; // allow native editing
            }

            // Expand/collapse single selected node with ArrowRight/ArrowLeft
            if (e.key === 'ArrowRight' || e.key === 'ArrowLeft') {
                const sel = selectedPathRef.current;
                if (!sel) return;
                const toggleControl = (allToggleButtonsRef.current as any[]).find((tc: any) => tc.path === sel);
                if (!toggleControl) return; // leaf node or not found
                const wantExpand = e.key === 'ArrowRight';
                if (wantExpand && !toggleControl.isExpanded()) {
                    toggleControl.setExpanded(true);
                } else if (!wantExpand && toggleControl.isExpanded()) {
                    toggleControl.setExpanded(false);
                } else {
                    return; // no change
                }
                // Recompute overall expansion state
                const total = (allToggleButtonsRef.current as any[]).length;
                const expandedCount = (allToggleButtonsRef.current as any[]).reduce((acc: number, tc: any) => acc + (tc.isExpanded && tc.isExpanded() ? 1 : 0), 0);
                setAllExpanded(total > 0 && expandedCount === total);
                e.preventDefault();
                e.stopPropagation();
                return;
            }

            // Arrow navigation among visible tree nodes
            if (e.key === 'ArrowUp' || e.key === 'ArrowDown') {
                if (!contentsRef.current) return;
                // Collect only visible tree nodes (skip those inside collapsed subtrees).
                const nodes = (Array.from(contentsRef.current.querySelectorAll('.tree-node')) as HTMLElement[])
                    .filter(n => n.offsetParent !== null); // offsetParent null => hidden via display:none
                if (!nodes.length) return;
                let idx = nodes.findIndex(n => n.getAttribute('data-path') === selectedPathRef.current);
                if (idx === -1) {
                    idx = e.key === 'ArrowDown' ? -1 : nodes.length; // start before/after bounds
                }
                const nextIdx = e.key === 'ArrowDown' ? Math.min(idx + 1, nodes.length - 1) : Math.max(idx - 1, 0);
                if (nextIdx !== idx) {
                    // Clear previous selection styling
                    if (selectedPathRef.current) {
                        const prevSel = contentsRef.current.querySelector('.tree-node.selected');
                        if (prevSel) prevSel.classList.remove('selected');
                    }
                    const el = nodes[nextIdx];
                    el.classList.add('selected');
                    selectedPathRef.current = el.getAttribute('data-path') || '';
                    el.scrollIntoView({ block: 'nearest' });
                    e.preventDefault();
                    e.stopPropagation();
                }
                return; // handled arrow key
            }

            // Expand / collapse all descendants shortcuts
            if (e.key !== '+' && e.key !== '=' && e.key !== '-') return;
            const sel = selectedPathRef.current;
            if (!sel) return;
            const expand = (e.key === '+' || e.key === '=');
            let affected = 0;
            (allToggleButtonsRef.current as any[]).forEach((tc: any) => {
                if (!tc.path) return;
                if (tc.path === sel || tc.path.startsWith(sel + '.')) {
                    tc.setExpanded(expand);
                    affected++;
                }
            });
            if (affected > 0) {
                const total = (allToggleButtonsRef.current as any[]).length;
                const expandedCount = (allToggleButtonsRef.current as any[]).reduce((acc: number, tc: any) => acc + (tc.isExpanded && tc.isExpanded() ? 1 : 0), 0);
                setAllExpanded(total > 0 && expandedCount === total);
                e.preventDefault();
                e.stopPropagation();
            }
        };
        window.addEventListener('keydown', handler);
        return () => window.removeEventListener('keydown', handler);
    }, [rawMode, data]);

    const handleToggleAll = () => {
        const newState = !allExpanded;
        setAllExpanded(newState);
        allToggleButtonsRef.current.forEach((toggleControl: any) => {
            toggleControl.setExpanded(newState);
        });
    };

    return (
        <div
            className="inspect-overlay"
            onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}
        >
            <div className="inspect-container">
                <div className="inspect-filter-bar">
                    <div className="inspect-filter-left">
                        <button
                            type="button"
                            className="inspect-close"
                            onClick={onClose}
                            aria-label="Close inspect view"
                            title="Close"
                        >
                            ×
                        </button>
                        <div className="inspect-test-name" title={fileName}>{fileName}</div>
                    </div>
                    <div className="inspect-search-controls">
                        {!rawMode && (
                            <button
                                className="inspect-toggle-all"
                                onClick={handleToggleAll}
                                title={allExpanded ? "Collapse all" : "Expand all"}
                            >
                                {allExpanded ? '><' : '<>'}
                            </button>
                        )}
                        <SearchInput
                            value={filter}
                            onChange={setFilter}
                            usePersistentSearching={false}
                        />
                    </div>
                </div>
                <div className="inspect-scroll" ref={contentsRef}>
                    {/* Raw mode content injected directly into contentsRef */}
                </div>
            </div>
        </div>
    );
};

// ---------------- Formatting / Utilities ----------------

function formatValue(v: InspectPrimitive): string {
    switch (v.type) {
        case 'string':
        case 'boolean':
        case 'number':
        case 'bytes': return String(v.value);
        case 'unevaluated': return '⏳';
        case 'error': return `❌ ${v.value}`;
    }
}

/**
 * Creates an HTML element with the specified tag, attributes, and children.
 *
 * @param tag - The HTML tag name to create (e.g., 'div', 'span').
 * @param attrs - An object containing attribute key-value pairs to set on the element.
 *                Special handling for 'class' (sets `className`) and 'style' (assigns to `style`).
 * @param children - A list of child nodes or strings to append as children of the element.
 *                   Strings are converted to text nodes.
 * @returns The constructed HTMLElement with the given attributes and children.
 */
function node(tag: string, attrs: Record<string, any>, ...children: (string | Node)[]): HTMLElement {
    const el = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs)) {
        if (k === 'class') el.className = v;
        else if (k === 'style') Object.assign(el.style, v);
        else el.setAttribute(k, v);
    }
    for (const child of children) {
        if (typeof child === 'string') el.appendChild(document.createTextNode(child));
        else el.appendChild(child);
    }
    return el;
}

/**
 * Highlights all occurrences of the given filter terms within a string by wrapping them in a span element
 * with a "highlight" class. Multiple terms are supported and matches are case-insensitive.
 * Overlapping matches are merged to avoid nested highlights.
 *
 * @param str - The input string to search and highlight.
 * @param filter - The filter string containing one or more space-separated terms to highlight.
 * @returns An HTMLElement containing the highlighted string, or the original string if no matches are found or filter is empty.
 */
function highlightMatch(str: string, filter: string): HTMLElement | string {
    if (!filter) return str;

    // Split filter into multiple terms by spaces
    const terms = filter.trim().split(/\s+/).filter(t => t.length > 0);
    if (terms.length === 0) return str;

    // Build a list of all match positions for all terms
    const lowerStr = str.toLowerCase();
    const matches: Array<{ start: number; end: number; term: string }> = [];

    for (const term of terms) {
        const lowerTerm = term.toLowerCase();
        let searchStart = 0;
        while (true) {
            const index = lowerStr.indexOf(lowerTerm, searchStart);
            if (index === -1) break;
            matches.push({
                start: index,
                end: index + lowerTerm.length,
                term: term
            });
            searchStart = index + 1;
        }
    }

    // If no matches found, return the original string
    if (matches.length === 0) return str;

    // Sort matches by start position
    matches.sort((a, b) => a.start - b.start);

    // Merge overlapping matches
    const merged: Array<{ start: number; end: number }> = [];
    for (const match of matches) {
        if (merged.length === 0 || match.start > merged[merged.length - 1].end) {
            merged.push({ start: match.start, end: match.end });
        } else {
            // Extend the previous match if they overlap
            merged[merged.length - 1].end = Math.max(merged[merged.length - 1].end, match.end);
        }
    }

    // Build the result with highlighted segments
    const result = node('span', {});
    let lastEnd = 0;

    for (const match of merged) {
        // Add text before the match
        if (match.start > lastEnd) {
            result.appendChild(document.createTextNode(str.slice(lastEnd, match.start)));
        }
        // Add highlighted match
        result.appendChild(node('span', { class: 'highlight' }, str.slice(match.start, match.end)));
        lastEnd = match.end;
    }

    // Add remaining text after the last match
    if (lastEnd < str.length) {
        result.appendChild(document.createTextNode(str.slice(lastEnd)));
    }

    return result;
}

/**
 * Renders raw text content (when rawMode is enabled) into the provided container ref.
 * Applies multi-term AND filtering and highlights matched substrings.
 * Whitespace is preserved and lines are displayed in a monospace font.
 *
 * @param contentsRef - Ref to the scroll/content div that will receive rendered children
 * @param rawText - The full raw text loaded from the target file
 * @param filter - The user-entered filter string (space-separated terms; all must match)
 */
function renderRawTextContent(
    contentsRef: React.RefObject<HTMLDivElement | null>,
    rawText: string,
    filter: string
): void {
    if (!contentsRef.current) return;

    const container = document.createElement('div');
    container.style.fontFamily = 'monospace';

    const terms = filter.trim().split(/\s+/).filter(t => t.length > 0);
    const lowerTerms = terms.map(t => t.toLowerCase());
    const lines = rawText ? rawText.split(/\r?\n/) : [];

    for (let i = 0; i < lines.length; i++) {
        const line = lines[i];
        if (lowerTerms.length > 0) {
            const matchAll = lowerTerms.every(term => line.toLowerCase().includes(term));
            if (!matchAll) continue; // AND filtering semantics similar to tree filter
        }
        const div = document.createElement('div');
        div.style.whiteSpace = 'pre';
        const highlighted = highlightMatch(line, filter);
        if (typeof highlighted === 'string') div.textContent = highlighted; else div.appendChild(highlighted);
        container.appendChild(div);
    }

    if (!container.childElementCount) {
        const empty = document.createElement('div');
        empty.textContent = 'No matches';
        container.appendChild(empty);
    }

    contentsRef.current.replaceChildren(container);
}

/**
 * Handles parsed (tree) mode rendering including filter application, auto-expansion during
 * active searches, and restoration of pre-search expansion state once cleared.
 *
 * @param data Root inspect object data (nullable until fetched)
 * @param error Current error (if any) aborts rendering
 * @param filter Current user filter string (space-separated AND terms)
 * @param contentsRef Ref to container into which tree is rendered
 * @param selectedPathRef Ref tracking the currently selected node path
 * @param allToggleButtonsRef Ref aggregating toggle controls for expansion operations
 * @param preSearchExpandedRef Snapshot of expanded paths prior to starting a search
 * @param clearedSearchRef Flag indicating a transition from searching -> cleared
 * @param setAllExpanded React state setter for global expand/collapse state indicator
 */
function renderParsedModeContent(
    data: InspectObject | null,
    error: string | null,
    filter: string,
    contentsRef: React.RefObject<HTMLDivElement | null>,
    selectedPathRef: React.MutableRefObject<string>,
    allToggleButtonsRef: React.MutableRefObject<any[]>,
    preSearchExpandedRef: React.MutableRefObject<Set<string>>,
    clearedSearchRef: React.MutableRefObject<boolean>,
    setAllExpanded: React.Dispatch<React.SetStateAction<boolean>>
): void {
    if (!contentsRef.current || !data || error) return;

    // Reset toggle controls collection before rebuilding tree
    allToggleButtonsRef.current = [] as any[];
    const hasFilter = filter.trim().length > 0;

    updateFilteredTree(data, filter, contentsRef, selectedPathRef, allToggleButtonsRef);

    if (hasFilter) {
        // Auto-expand all nodes when filter active so user can see matches immediately.
        (allToggleButtonsRef.current as any[]).forEach((toggleControl: any) => {
            toggleControl.setExpanded(true);
        });
        setAllExpanded(true);
        return;
    }

    // No active filter: either restoring pre-search expansion snapshot or leaving defaults.
    if (clearedSearchRef.current) {
        (allToggleButtonsRef.current as any[]).forEach((tc: any) => {
            if (tc.path && preSearchExpandedRef.current.has(tc.path)) {
                tc.setExpanded(true);
            }
        });
        const total = (allToggleButtonsRef.current as any[]).length;
        const expandedCount = (allToggleButtonsRef.current as any[]).reduce(
            (acc: number, tc: any) => acc + (tc.isExpanded && tc.isExpanded() ? 1 : 0),
            0
        );
        setAllExpanded(total > 0 && expandedCount === total);
        clearedSearchRef.current = false;
    } else {
        // Default collapsed state when not searching and not restoring.
        setAllExpanded(false);
    }
}

// ---------------- Tree Rendering Functions ----------------

/**
 * Creates a click handler for tree node selection.
 * Handles clicking on tree nodes to select them and deselect the previous selection.
 * 
 * @param contentsRef - Reference to the main contents container
 * @param selectedPathRef - Reference to the currently selected path
 * @returns The click event handler function
 */
function createTreeNodeClickHandler(
    contentsRef: React.RefObject<HTMLDivElement | null>,
    selectedPathRef: React.MutableRefObject<string>
) {
    return (e: MouseEvent) => {
        const target = e.target as HTMLElement;
        const n = target.closest('.tree-node');
        if (n) {
            const path = n.getAttribute('data-path');
            if (path && contentsRef.current) {
                // Clear previous selection
                if (selectedPathRef.current) {
                    const prevSelected = contentsRef.current.querySelector(
                        `.tree-node[data-path="${CSS.escape(selectedPathRef.current)}"]`
                    );
                    if (prevSelected) {
                        prevSelected.classList.remove('selected');
                    }
                }
                // Set new selection
                selectedPathRef.current = path;
                n.classList.add('selected');
            }
        }
    };
}

/**
 * Creates a toggle control object for managing expand/collapse state of a tree node.
 * @param toggle - The toggle button element
 * @param subtree - The subtree container element
 * @returns An object with methods to control the toggle state
 */
function createToggleControl(toggle: HTMLElement, subtree: HTMLElement) {
    let expanded = false;

    return {
        toggle,
        subtree,
        setExpanded: (val: boolean) => {
            expanded = val;
            toggle.textContent = expanded ? '[-]' : '[+]';
            subtree.style.display = expanded ? '' : 'none';
        },
        isExpanded: () => expanded,
        // path will be attached later by the caller (createObjectNodeHeader)
        path: undefined as string | undefined,
    };
}

/**
 * Creates a tree node header with expand/collapse functionality for object nodes.
 * @param key - The property key name
 * @param filterLower - The lowercase filter string for highlighting
 * @param indent - The indentation string for this depth level
 * @param fullPath - The full dot-notation path to this node
 * @param subtree - The subtree container element
 * @param contentsRef - Reference to the main contents container
 * @param selectedPathRef - Reference to the currently selected path
 * @param allToggleButtonsRef - Reference to array of all toggle controls
 * @returns Object containing the header element and toggle control
 */
function createObjectNodeHeader(
    key: string,
    filterLower: string,
    indent: string,
    fullPath: string,
    subtree: HTMLElement,
    contentsRef: React.RefObject<HTMLDivElement | null>,
    selectedPathRef: React.MutableRefObject<string>,
    allToggleButtonsRef: React.MutableRefObject<any[]>
) {
    const toggle = node('span', { class: 'tree-expander', style: { cursor: 'pointer' } }, '[+]');
    const header = node('div',
        { class: 'tree-node', style: { marginLeft: indent }, 'data-path': fullPath },
        toggle,
        node('span', { class: 'tree-key' }, highlightMatch(key, filterLower))
    );

    const toggleControl = createToggleControl(toggle, subtree);
    // Attach path so we can snapshot/restore expansion state across searches.
    (toggleControl as any).path = fullPath;
    allToggleButtonsRef.current.push(toggleControl);
    
    // Initialize the subtree as collapsed
    toggleControl.setExpanded(false);

    // Handle toggle click
    toggle.addEventListener('click', (e) => {
        e.stopPropagation(); // Prevent click from bubbling to parent tree-node

        // Toggle the expanded state
        const currentlyExpanded = toggle.textContent === '[-]';
        toggleControl.setExpanded(!currentlyExpanded);

        // Select this row when toggling
        if (contentsRef.current && selectedPathRef.current) {
            const prevSelected = contentsRef.current.querySelector(`.tree-node[data-path="${CSS.escape(selectedPathRef.current)}"]`);
            if (prevSelected) {
                prevSelected.classList.remove('selected');
            }
        }
        selectedPathRef.current = fullPath;
        header.classList.add('selected');
    });

    return { header, toggleControl };
}

/**
 * Creates a leaf node (primitive value) in the tree.
 * @param key - The property key name
 * @param valText - The formatted value text
 * @param filterLower - The lowercase filter string for highlighting
 * @param indent - The indentation string for this depth level
 * @param fullPath - The full dot-notation path to this node
 * @returns The leaf node element
 */
function createLeafNode(
    key: string,
    valText: string,
    filterLower: string,
    indent: string,
    fullPath: string
): HTMLElement {
    return node('div',
        { class: 'tree-node', style: { marginLeft: indent }, 'data-path': fullPath },
        node('span', { class: 'tree-key' }, highlightMatch(`${key}: `, filterLower)),
        node('span', {}, highlightMatch(valText, filterLower))
    );
}

/**
 * Recursively renders an inspect node and its children as a DOM tree.
 * Filters nodes based on the filter string and handles expand/collapse for object nodes.
 * 
 * @param nodeData - The inspect node data to render
 * @param filterLower - Lowercase filter string for matching/highlighting
 * @param path - Current dot-notation path (for tracking selection)
 * @param alreadyMatched - Whether a parent node matched the filter (show all children)
 * @param depth - Current depth level (for indentation)
 * @param contentsRef - Reference to the main contents container
 * @param selectedPathRef - Reference to the currently selected path
 * @param allToggleButtonsRef - Reference to array of all toggle controls
 * @returns The rendered tree container element, or null if filtered out
 */
function renderInspectNode(
    nodeData: InspectNode,
    filterLower: string,
    path: string,
    alreadyMatched: boolean,
    depth: number,
    contentsRef: React.RefObject<HTMLDivElement | null>,
    selectedPathRef: React.MutableRefObject<string>,
    allToggleButtonsRef: React.MutableRefObject<any[]>
): HTMLElement | null {
    if (nodeData.type !== 'object') return null;

    const container = node('div', { class: 'tree-children' });

    // Split filter into multiple terms
    const filterTerms = filterLower ? filterLower.split(/\s+/).filter(t => t.length > 0) : [];

    // Process each child of this object
    for (const child of nodeData.children) {
        const key = child.key;
        const valNode = child.value;
        const keyLower = key.toLowerCase();
        const valText = valNode.type === 'object' ? '' : formatValue(valNode);
        const valLower = valText.toLowerCase();

        // Combine key and value for searching (AND logic - all terms must match)
        const combinedText = `${keyLower} ${valLower}`;

        // Check if ALL terms match somewhere in the combined key+value text (AND logic)
        const allTermsMatch = filterTerms.length === 0 || filterTerms.every(term => combinedText.includes(term));

        const indent = `${depth * 1.2}em`;
        const fullPath = path ? `${path}.${key}` : key;

        if (valNode.type === 'object') {
            // Recursively render object children
            const subtree = renderInspectNode(
                valNode,
                filterLower,
                fullPath,
                allTermsMatch || alreadyMatched,
                depth + 1,
                contentsRef,
                selectedPathRef,
                allToggleButtonsRef
            );

            if (subtree) {
                const { header } = createObjectNodeHeader(
                    key,
                    filterLower,
                    indent,
                    fullPath,
                    subtree,
                    contentsRef,
                    selectedPathRef,
                    allToggleButtonsRef
                );
                container.append(header, subtree);
            }
        } else if (filterTerms.length === 0 || allTermsMatch || alreadyMatched) {
            // Render leaf node (primitive value) if it matches the filter
            container.append(createLeafNode(key, valText, filterLower, indent, fullPath));
        }
    }

    return container.children.length > 0 ? container : null;
}

/**
 * Renders the complete filtered tree and handles selection restoration.
 * This is the main entry point for tree rendering.
 * 
 * @param data - The root inspect object data
 * @param filter - The current filter string
 * @param contentsRef - Reference to the main contents container
 * @param selectedPathRef - Reference to the currently selected path
 * @param allToggleButtonsRef - Reference to array of all toggle controls
 */
function updateFilteredTree(
    data: InspectObject,
    filter: string,
    contentsRef: React.RefObject<HTMLDivElement | null>,
    selectedPathRef: React.MutableRefObject<string>,
    allToggleButtonsRef: React.MutableRefObject<any[]>
): void {
    if (!contentsRef.current || !data) return;

    const filterLower = filter.trim().toLowerCase();

    // Render the tree with the current filter
    const filtered = renderInspectNode(
        data,
        filterLower,
        '',
        false,
        0,
        contentsRef,
        selectedPathRef,
        allToggleButtonsRef
    );

    // Replace the contents with the newly filtered tree
    contentsRef.current.replaceChildren(filtered || node('div', {}, 'No matches'));

    // Restore the previous selection if it still exists in the filtered tree
    if (selectedPathRef.current) {
        const anchor = contentsRef.current.querySelector(
            `.tree-node[data-path="${CSS.escape(selectedPathRef.current)}"]`
        );
        if (anchor) {
            anchor.classList.add('selected');
            // Scroll to the selected element after the next paint
            requestAnimationFrame(() => {
                if (anchor) {
                    (anchor as HTMLElement).scrollIntoView({ block: 'center' });
                }
            });
        }
    }
}

// ---------------- Serialization / Lookup Helpers ----------------

/**
 * Resolve a dot-delimited path (e.g. "root.child.sub") into an InspectNode within the given root object.
 * Returns null if any path segment is missing.
 */
function getNodeByPath(root: InspectObject, path: string): InspectNode | null {
    if (path === '') return root;
    const segments = path.split('.');
    let current: InspectNode = root;
    for (let i = 0; i < segments.length; i++) {
        const seg = segments[i];
        if (current.type !== 'object') return null; // cannot descend into primitive
        const obj: InspectObject = current; // narrow type
        const childEntry = obj.children.find((c: { key: string; value: InspectNode }) => c.key === seg);
        if (!childEntry) return null;
        current = childEntry.value;
    }
    return current;
}

/**
 * Convert an InspectNode subtree into a plain JS structure for copying. Object nodes become nested objects.
 * Primitive nodes convert based on type. Unevaluated becomes a string placeholder; error becomes { error: value }.
 */
// (Deprecated) retained for future use if JSON serialization is needed again.
// function serializeInspectNode(node: InspectNode): any { /* removed in favor of formatNodeWithRootName */ }

/**
 * Produce a textual representation including the root key name, mimicking a lightweight
 * structured dump (key { ... } or key: value). Indents with two spaces per depth.
 */
function formatNodeWithRootName(rootKey: string, node: InspectNode): string {
    const lines: string[] = [];
    const IND = '  ';
    function emitPrimitive(key: string, prim: InspectPrimitive, depth: number) {
        let val: string;
        switch (prim.type) {
            case 'string': val = JSON.stringify(prim.value); break;
            case 'bytes': val = JSON.stringify(prim.value); break;
            case 'boolean': val = String(prim.value); break;
            case 'number': val = prim.value; break;
            case 'error': val = `ERROR(${JSON.stringify(prim.value)})`; break;
            case 'unevaluated': val = '(unevaluated)'; break;
        }
        lines.push(`${IND.repeat(depth)}${key}: ${val}`);
    }
    function walk(key: string, n: InspectNode, depth: number) {
        if (n.type === 'object') {
            lines.push(`${IND.repeat(depth)}${key} {`);
            for (const child of n.children) {
                walk(child.key, child.value, depth + 1);
            }
            lines.push(`${IND.repeat(depth)}}`);
        } else {
            emitPrimitive(key, n as InspectPrimitive, depth);
        }
    }
    walk(rootKey, node, 0);
    return lines.join('\n');
}
