// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import React from "react";

export function ShortcutHeader({
  title,
}: {
  title: string;
}): React.JSX.Element {
  return (
    <h2 className="mt-2 mb-2 flex items-center gap-3 font-mono text-[1.2rem] font-bold">
      <span className="whitespace-nowrap">{title}</span>
      <span
        className="h-[1px] flex-1 rounded bg-black translate-y-[1px]"
        aria-hidden="true"
      />
    </h2>
  );
}
