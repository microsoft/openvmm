// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import React from "react";
import { Link } from "react-router-dom";
import { Menu } from "../menu";
import "../styles/common.css";
import "../../tailwind.css";
import { ShortcutHeader } from "./header";
import { ShortcutRow } from "./shortcut";
import { shortcutSections } from "./shortcuts";

export function Docs(): React.JSX.Element {
  return (
    <div className="common-page-display">
      <div className="common-page-header">
        <div className="common-header-left">
          <div className="common-header-title">
            <Menu />
            <Link to="/docs" className="common-header-path">
              Docs
            </Link>
          </div>
        </div>
      </div>
      {/* Display the shortcuts list defined in shortcuts.ts */}
      <div className="pb-8 pt-1">
        {shortcutSections.map((section) => (
          <div className="pb-8">
            <ShortcutHeader title={section.title} />

            {section.note && (
              <div className="mb-2 text-sm text-gray-700">{section.note}</div>
            )}

            <table className="border-collapse table-fixed w-full">
              <tbody className="divide-y divide-solid divide-gray-200">
                {section.shortcuts.map((s) => (
                  <ShortcutRow
                    shortcutParts={s.shortcutParts}
                    descriptions={s.descriptions}
                  />
                ))}
              </tbody>
            </table>
          </div>
        ))}
      </div>
    </div>
  );
}
