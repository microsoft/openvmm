// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { ColumnDef } from '@tanstack/react-table';
import { LogEntry } from '../data_defs';

export const defaultSorting = [
  { id: "relative", desc: false }, // Sort by status ascending, failed tests first
];

export const columnWidthMap = {
    relative: 110,
    severity: 80,
    source: 100,
    screenshot: 100,
};

export function createColumns(
    setModalContent: (content: string | null) => void
): ColumnDef<LogEntry>[] {
    return [
        {
            accessorKey: 'relative',
            header: 'Timestamp',
            cell: (info) => (
                <span title={info.row.original.timestamp}>
                    {info.getValue() as string}
                </span>
            ),
            enableSorting: true,
        },
        {
            accessorKey: 'severity',
            header: 'Severity',
            enableSorting: false,
        },
        {
            accessorKey: 'source',
            header: 'Source',
            enableSorting: false,
        },
        {
            id: 'message',
            accessorFn: (row) => row.message, // Use text for sorting/filtering
            header: 'Message',
            cell: (info) => (
                // NOTE: React normally escapes HTML to prevent XSS attacks.
                // Using dangerouslySetInnerHTML bypasses that protection.
                // This message data is NOT user controlled and comes from the
                // logs. We add a link to the inspect attachment so we need to
                // treat this as html.
                <div dangerouslySetInnerHTML={{ __html: info.row.original.message }} />
            ),
            enableSorting: false, // Disable sorting for complex HTML content
        },
        {
            id: 'screenshot',
            header: 'Screenshot',
            cell: (info) => {
                const screenshot = info.row.original.screenshot;
                return screenshot ? (
                    <img
                        src={screenshot}
                        alt="Screenshot"
                        style={{
                            maxWidth: '100px',
                            maxHeight: '50px',
                            cursor: 'pointer',
                            objectFit: 'contain'
                        }}
                        onClick={(e) => {
                            e.stopPropagation();
                            setModalContent(screenshot);
                        }}
                    />
                ) : '';
            },
            enableSorting: false,
        }
    ];
}
