// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { ColumnDef } from '@tanstack/react-table';
import { LogEntry } from '../data_defs';
import { AnsiSpan } from '../ansi_span';

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
            accessorFn: (row) => row.logMessage, // Use text for sorting/filtering
            header: 'Message',
            cell: (info) => (
                <>
                    <div><AnsiSpan text={info.row.original.logMessage.rawMessage} /></div>
                    {info.row.original.logMessage.links?.map((link, idx) => (
                        <a
                            key={idx}
                            href={link.url}
                            className="attachment"
                            target="_blank"
                            rel="noopener noreferrer"
                            data-inspect={link.inspect}
                            style={{ marginLeft: 8 }}
                        >
                            {link.text}
                        </a>
                    ))}
                </>
            ),
            enableSorting: false, // Sorting by full message text is not useful
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
