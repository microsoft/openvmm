// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import React, { useEffect, useRef } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import "./styles/common.css";

interface SearchInputProps {
  value: string;
  onChange: (value: string) => void;
  inputRef?: React.RefObject<HTMLInputElement | null>;
  /**
   * When true (default), the component installs global key handlers for:
   *  - Ctrl/Cmd+F to focus/select the input
   *  - Escape to clear (or blur when empty)
   * When false, these handlers are disabled entirely.
   *
   * This allows parent components (e.g. the log viewer) to temporarily
   * suspend search keyboard behavior while an overlay (like Inspect) is open.
   */
  active?: boolean;
  /**
   * When true (default), the component syncs its value to the `?search=` URL param
   * and initializes from it on mount. Set false for ephemeral searches (e.g. Inspect overlay).
   */
  usePersistentSearching?: boolean;
}

export function SearchInput({
  value,
  onChange,
  inputRef,
  active = true,
  usePersistentSearching = true,
}: SearchInputProps): React.JSX.Element {
  const location = useLocation();
  const navigate = useNavigate();
  const isInitialMount = useRef(true);
  const internalRef = useRef<HTMLInputElement>(null);
  const actualRef = inputRef || internalRef;

  // On mount: read search parameter from URL and update caller's filter (only if persistent searching is enabled)
  useEffect(() => {
    if (!usePersistentSearching) {
      isInitialMount.current = false;
      return;
    }

    const params = new URLSearchParams(location.search);
    const searchParam = params.get("search");
    if (searchParam !== null && searchParam !== value) {
      onChange(searchParam);
    }
    isInitialMount.current = false;
  }, []); // Only run on mount

  // When value changes (after initial mount), update the URL (only if persistent searching is enabled)
  useEffect(() => {
    if (!usePersistentSearching) return;
    if (isInitialMount.current) return; // Skip on initial mount

    const params = new URLSearchParams(location.search);
    if (value) {
      params.set("search", value);
    } else {
      params.delete("search");
    }

    const newSearch = params.toString();
    const newPath = newSearch
      ? `${location.pathname}?${newSearch}`
      : location.pathname;

    // Only navigate if the URL actually changed
    if (location.pathname + location.search !== newPath) {
      navigate(newPath, { replace: true });
    }
  }, [
    value,
    location.pathname,
    navigate,
    location.search,
    usePersistentSearching,
  ]);

  // Handle Ctrl/Cmd+F keyboard shortcut and Escape to clear/blur (only if active)
  useEffect(() => {
    if (!active) return; // Skip installing handlers when inactive

    const handleKeyDown = (e: KeyboardEvent) => {
      const isMac = navigator.platform.toUpperCase().includes("MAC");
      const isFind =
        (e.key === "f" || e.key === "F") &&
        ((isMac && e.metaKey) || (!isMac && e.ctrlKey));

      if (isFind && document.activeElement !== actualRef.current) {
        e.preventDefault();
        actualRef.current?.focus();
        actualRef.current?.select();
      }

      if (e.key === "Escape") {
        if (value) {
          // If there's text, clear it
          onChange("");
        } else if (document.activeElement === actualRef.current) {
          // If empty and search bar is focused, remove focus from it
          actualRef.current?.blur();
        }
      }
    };

    document.addEventListener("keydown", handleKeyDown);
    return () => document.removeEventListener("keydown", handleKeyDown);
  }, [value, onChange, active]);

  return (
    <div style={{ display: "inline-block" }}>
      <input
        ref={actualRef}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder="Filter ..."
        className="common-search-input"
        // aria-disabled hints to assistive tech that shortcuts are not active
        aria-disabled={!active}
      />
      {value && (
        <button
          onClick={() => onChange("")}
          className="common-search-clear-btn"
          title="Clear filter"
        >
          ×
        </button>
      )}
    </div>
  );
}
