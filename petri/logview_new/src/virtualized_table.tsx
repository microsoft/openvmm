// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import React, { useState, useEffect, useRef, useLayoutEffect } from "react";
import { useVirtualizer } from "@tanstack/react-virtual";
import {
  flexRender,
  type Row,
  type ColumnDef,
  type SortingState,
  useReactTable,
  getCoreRowModel,
  getSortedRowModel,
  getFilteredRowModel,
} from "@tanstack/react-table";
import "./styles/virtualized_table.css";

export interface VirtualizedTableProps<TData extends object> {
  data: TData[];
  columns: ColumnDef<TData, any>[];
  sorting: SortingState;
  onSortingChange: (
    updater: SortingState | ((old: SortingState) => SortingState)
  ) => void;
  columnWidthMap: Record<string, number>;
  estimatedRowHeight?: number; // default 50
  overscan?: number; // default 10
  /** Derive a className for a given row (virtual wrapper div). */
  getRowClassName?: (row: Row<TData>) => string;
  /** Handle row click events */
  onRowClick?: (row: Row<TData>, event: React.MouseEvent) => void;
  /** If provided, the virtualizer will scroll this row index into view (center aligned). */
  scrollToIndex?: number | null;
}

function defaultInferRowClass(row: Row<any>): string {
  const failed = row?.original?.metadata?.petriFailed;
  if (typeof failed === "number") {
    return failed > 0 ? "failed-row" : "passed-row";
  }
  return "passed-row";
}

export function VirtualizedTable<TData extends object>({
  data,
  columns,
  sorting,
  onSortingChange,
  columnWidthMap,
  estimatedRowHeight = 100,
  overscan = 20,
  getRowClassName,
  onRowClick,
  scrollToIndex,
}: VirtualizedTableProps<TData>): React.JSX.Element {
  const table = useReactTable({
    data,
    columns,
    state: {
      sorting,
    },
    onSortingChange,
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getFilteredRowModel: getFilteredRowModel(),
    enableSorting: true,
    enableSortingRemoval: false,
    debugTable: false,
  });

  const { rows } = table.getRowModel();

  const tableContainerRef = useRef<HTMLDivElement>(null);
  const headerWrapperRef = useRef<HTMLDivElement>(null);
  const [headerHeight, setHeaderHeight] = useState(25.5); // Initial estimate

  // Measure the header and set the value appropriately
  useLayoutEffect(() => {
    const el = headerWrapperRef.current;
    if (!el) return;
    setHeaderHeight(el.getBoundingClientRect().height);
  }, []);

  const rowVirtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => tableContainerRef.current,
    estimateSize: () => estimatedRowHeight,
    overscan,
    measureElement:
      typeof window !== "undefined" &&
        navigator.userAgent.indexOf("Firefox") === -1
        ? (element) => element?.getBoundingClientRect().height
        : undefined,
  });

  // Force recompute when data/rows change (e.g., during filtering/searching).
  // This ensures the virtualizer knows about new heights if the data changes.
  useEffect(() => {
    rowVirtualizer.calculateRange();
    rowVirtualizer.getVirtualItems().forEach((virtualRow) => {
      const el = document.querySelector(`[data-index="${virtualRow.index}"]`);
      if (el) {
        rowVirtualizer.measureElement(el);
      }
    });
  }, [rows.length, data, rowVirtualizer, sorting]);

  // Scroll to a requested index (center align) whenever scrollToIndex changes.
  useEffect(() => {
    if (scrollToIndex == null) return;
    if (scrollToIndex < 0 || scrollToIndex >= rows.length) return;
    try {
      rowVirtualizer.scrollToIndex(scrollToIndex, { align: "center" });
    } catch {
      /* no-op */
    }
  }, [scrollToIndex, rowVirtualizer, rows.length]);

  return (
    <div>
      <div
        ref={headerWrapperRef}
        className="virtualized-table-header-container"
      >
        <table className="virtualized-table">
          <thead>
            {table.getHeaderGroups().map((headerGroup) => (
              <tr key={headerGroup.id}>
                {headerGroup.headers.map((header) => {
                  return (
                    <th
                      key={header.id}
                      className={header.column.getCanSort() ? "sortable" : ""}
                      onClick={header.column.getToggleSortingHandler()}
                      style={{
                        width: columnWidthMap[header.column.id],
                      }}
                    >
                      <div className="virtualized-table-header-content">
                        {header.isPlaceholder
                          ? null
                          : flexRender(
                            header.column.columnDef.header,
                            header.getContext()
                          )}
                        {header.column.getCanSort() && (
                          <span className="sort-indicator">
                            {{
                              asc: "↑",
                              desc: "↓",
                            }[header.column.getIsSorted() as string] ?? "⇅"}
                          </span>
                        )}
                      </div>
                    </th>
                  );
                })}
              </tr>
            ))}
          </thead>
        </table>
      </div>
      <div
        ref={tableContainerRef}
        className="virtualized-table-body"
        style={{
          height: `calc(100vh - 3.2rem - ${headerHeight}px)`,
        }}
      >
        <table className="virtualized-table">
          {/* No thead here if you already rendered a fixed header above */}
          <tbody
            // Make tbody the positioning context for absolute rows
            style={{
              position: 'relative',
              display: 'block',                // allow custom height/scroll
              height: `${rowVirtualizer.getTotalSize()}px`,
              width: '100%',
            }}
          >
            {rowVirtualizer.getVirtualItems().map((virtualRow) => {
              const row = rows[virtualRow.index] as Row<TData>;
              return (
                <tr
                  key={row.id}
                  data-index={virtualRow.index}
                  ref={rowVirtualizer.measureElement}
                  className={`virtualized-table-row ${getRowClassName ? getRowClassName(row) : defaultInferRowClass(row)}`}
                  style={{
                    position: "absolute",
                    width: "100%",
                    transform: `translateY(${virtualRow.start}px)`,
                    display: "table",
                    tableLayout: "fixed",
                    boxSizing: "border-box",
                  }}
                  onClick={
                    onRowClick ? (event) => onRowClick(row, event) : undefined
                  }
                >
                  {row.getVisibleCells().map((cell) => {
                    return (
                      <td
                        key={cell.id}
                        style={{
                          boxSizing: "border-box",
                          width: columnWidthMap[cell.column.id],
                        }}
                      >
                        {flexRender(
                          cell.column.columnDef.cell,
                          cell.getContext()
                        )}
                      </td>
                    );
                  })}
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}
