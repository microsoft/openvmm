// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import React from 'react';

export interface VerifyPromptProps {
    title?: string;
    message?: string;
    onOk: () => void;
}

export function VerifyPrompt({
    title="Analyze tests from all branches?",
    message="This is a heavy operation that can take longer than 2 minutes to complete. Are you sure you want to analyze all?",
    onOk,
}: VerifyPromptProps): React.JSX.Element | null {
    return (
        <div
            className="flex items-center justify-center p-4 mt-10"
        >
            <div
                className="w-full max-w-xl rounded-lg bg-gray-200 p-6"
                onMouseDown={(e) => e.stopPropagation()}
            >
                <div className="text-lg font-semibold text-gray-900">{title}</div>
                <div className="mt-3 text-sm text-gray-700">{message}</div>

                <div className="mt-6 flex justify-end gap-3">
                    <button
                        type="button"
                        className="rounded-md bg-gray-800 px-6 py-3 text-sm font-semibold text-white border-none hover:cursor-pointer"
                        onClick={onOk}
                    >
                        OK
                    </button>
                </div>
            </div>
        </div>
    );
}
