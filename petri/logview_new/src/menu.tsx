// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import React, { useState, useCallback, useEffect } from "react";
import { createPortal } from "react-dom";
import { Link } from "react-router-dom";
import "./styles/menu.css";

// Menu component that opens from the left side
export function Menu(): React.JSX.Element {
  const [open, setOpen] = useState(false);
  const toggleVisible = useCallback(() => setOpen((o) => !o), []);

  // Prevent body scroll while drawer open
  useEffect(() => {
    if (open) {
      const prev = document.body.style.overflow;
      document.body.style.overflow = "hidden";
      return () => {
        document.body.style.overflow = prev;
      };
    }
  }, [open]);

  return (
    <>
      <button
        type="button"
        aria-label={open ? "Close navigation menu" : "Open navigation menu"}
        className="menu-trigger"
        onClick={toggleVisible}
      >
        <span className="menu-lines" aria-hidden="true">
          <span />
          <span />
          <span />
        </span>
      </button>
      {open &&
        createPortal(
          <>
            <div
              className="menu-overlay"
              onClick={toggleVisible}
              role="presentation"
            />
            <nav
              className={open ? "menu-drawer open" : "menu-drawer"}
              aria-hidden={!open}
              aria-label="Primary"
            >
              <div className="menu-drawer-header">Petri Test Viewer</div>
              <ul className="menu-nav-list" role="list">
                <li>
                  <Link
                    to="/runs"
                    className="drawer-link"
                    onClick={toggleVisible}
                  >
                    Runs
                  </Link>
                </li>
                <li>
                  <Link
                    to="/tests"
                    className="drawer-link"
                    onClick={toggleVisible}
                  >
                    Tests
                  </Link>
                </li>
                <li>
                  <Link
                    to="/docs"
                    className="drawer-link"
                    onClick={toggleVisible}
                  >
                    Docs
                  </Link>
                </li>
                <li className="drawer-separator" aria-hidden="true" />
                <li>
                  <a
                    className="drawer-link external"
                    href="https://github.com/microsoft/openvmm"
                    target="_blank" // Open in new window
                    rel="noopener noreferrer" // security best practice
                  >
                    Repo
                  </a>
                </li>
                <li>
                  <a
                    className="drawer-link external"
                    href="http://openvmm.dev/"
                    target="_blank"
                    rel="noopener noreferrer"
                  >
                    Guide
                  </a>
                </li>
              </ul>
            </nav>
          </>,
          document.body,
        )}
    </>
  );
}
