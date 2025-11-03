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
    const filterInputRef = useRef<HTMLInputElement>(null);
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
                    if (!r.ok) throw new Error(`${r.status} ${r.statusText}`);
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
            if (!contentsRef.current) return;
            // Render raw text lines with highlighting. Preserve whitespace.
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
            return;
        }

        // Parsed mode
        if (!contentsRef.current || !data || error) return;
        allToggleButtonsRef.current = [];
        const hasFilter = filter.trim().length > 0;
        updateFilteredTree(data, filter, contentsRef, selectedPathRef, allToggleButtonsRef);
        if (hasFilter) {
            // During an active search we auto-expand everything.
            (allToggleButtonsRef.current as any[]).forEach((toggleControl: any) => {
                toggleControl.setExpanded(true);
            });
            setAllExpanded(true);
        } else {
            // If we've just cleared a search, restore the pre-search expansion snapshot.
            if (clearedSearchRef.current) {
                (allToggleButtonsRef.current as any[]).forEach((tc: any) => {
                    if (tc.path && preSearchExpandedRef.current.has(tc.path)) {
                        tc.setExpanded(true);
                    }
                });
                // Compute aggregate allExpanded state after restoration.
                const total = (allToggleButtonsRef.current as any[]).length;
                const expandedCount = (allToggleButtonsRef.current as any[]).reduce((acc: number, tc: any) => acc + (tc.isExpanded && tc.isExpanded() ? 1 : 0), 0);
                setAllExpanded(total > 0 && expandedCount === total);
                clearedSearchRef.current = false; // reset flag
            } else {
                // No filter and not clearing from a search: leave default collapsed state.
                setAllExpanded(false);
            }
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
                <button className="inspect-close-btn" onClick={onClose} aria-label="Close Inspect">×</button>
                <div className="inspect-filter-bar">
                    <div className="inspect-test-name" title={fileName}>{fileName}</div>
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
                            inputRef={filterInputRef}
                            usePersistentSearching={false}
                        />
                    </div>
                </div>
                <div className="inspect-scroll" ref={contentsRef}>
                    {error && <div style={{ padding: '12px', color: 'red' }}>Error: {error}</div>}
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
