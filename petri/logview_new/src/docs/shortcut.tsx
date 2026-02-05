// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import React from "react";
import { Shortcut } from "./shortcuts";

// Render shortcut row in cheat sheet table
export function ShortcutRow({
  shortcutParts,
  descriptions,
}: Shortcut): React.JSX.Element {
  return (
    <tr className="border-x-0 text-[0.9rem]">
      <td className="w-[15rem] whitespace-nowrap align-top px-4 py-3 font-mono font-bold">
        {shortcutParts.map((part, i) => (
          <React.Fragment>
            {i > 0 && <span className="mx-1">+</span>}
            <kbd className="whitespace-nowrap inline-flex items-start rounded-md bg-gray-100 px-2 py-1">
              {part}
            </kbd>
          </React.Fragment>
        ))}
      </td>
      <td className="px-4 py-3 text-gray-800 text-[0.9rem]">
        <ul className="m-0 list-none pl-5">
          {descriptions.map((d) => (
            <li>{d}</li>
          ))}
        </ul>
      </td>
    </tr>
  );
}
