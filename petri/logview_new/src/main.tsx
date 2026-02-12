// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import React from "react";
import ReactDOM from "react-dom/client";
import { HashRouter } from "react-router-dom";
import "../tailwind.css";
import "./styles/main.css";
import { Routes, Route } from "react-router-dom";
import { Runs } from "./runs";
import { Navigate } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { startDataPrefetching } from "./fetch/fetch_runs_data";
import { RunDetails } from "./run_details";
import { Tests } from "./tests";
import { TestDetails } from "./test_details";
import { LogViewer } from "./log_viewer";
import { Docs } from "./docs/docs";
import { VerifyGetAllProvider } from "./contexts/verify_get_all_context";

const queryClient = new QueryClient();

// Start background data prefetching and refetching
startDataPrefetching(queryClient);

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <HashRouter>
      <QueryClientProvider client={queryClient}>
        <VerifyGetAllProvider>
          <Content />
        </VerifyGetAllProvider>
      </QueryClientProvider>
    </HashRouter>
  </React.StrictMode>,
);

function Content() {
  return (
    <Routes>
      <Route path="/" element={<Navigate to="/runs" replace />} />
      <Route path="runs" element={<Runs />} />
      <Route path="runs/:runId" element={<RunDetails />} />
      <Route path="tests" element={<Tests />} />
      <Route path="tests/:architecture/:testName" element={<TestDetails />} />
      <Route
        path="runs/:runId/:architecture/:testName"
        element={<LogViewer />}
      />
      <Route path="docs" element={<Docs />} />
    </Routes>
  );
}
