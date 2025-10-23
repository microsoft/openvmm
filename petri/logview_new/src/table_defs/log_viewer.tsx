import { ColumnDef } from '@tanstack/react-table';
import { LogEntry } from '../data_defs';

export const defaultSorting = [
  { id: "relative", desc: false }, // Sort by status ascending, failed tests first
];

export const columnWidthMap = {
    relative: 100,
    severity: 80,
    source: 80,
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
            accessorFn: (row) => row.messageText, // Use text for sorting/filtering
            header: 'Message',
            cell: (info) => (
                <div dangerouslySetInnerHTML={{ __html: info.row.original.messageHtml }} />
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
