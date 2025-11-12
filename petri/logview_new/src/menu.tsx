// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import React, { useState, useCallback, useEffect } from "react";
import { createPortal } from "react-dom";
import { useNavigate } from "react-router-dom";
import "./styles/menu.css";

// Menu component that opens from the left side
export function Menu(): React.JSX.Element {
  const [open, setOpen] = useState(false);
  const navigate = useNavigate();
  const toggle = useCallback(() => setOpen((o) => !o), []);

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

  function navigateAndClose(path: string) {
    navigate(path);
    toggle();
  }

  return (
    <>
      <button
        type="button"
        aria-label={open ? "Close navigation menu" : "Open navigation menu"}
        className="menu-trigger"
        onClick={toggle}
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
              onClick={toggle}
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
                  <button
                    className="drawer-link"
                    onClick={() => navigateAndClose("/runs")}
                  >
                    Runs
                  </button>
                </li>
                <li>
                  <button
                    className="drawer-link"
                    onClick={() => navigateAndClose('/tests')}
                  >
                    Tests
                  </button>
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
          document.body
        )}
    </>
  );
}
