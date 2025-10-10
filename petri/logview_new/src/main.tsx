import React from 'react';
import ReactDOM from 'react-dom/client';
import { HashRouter } from 'react-router-dom';
import { Routes, Route } from 'react-router-dom';
import { Navigate } from 'react-router-dom';
import { QueryClient, QueryClientProvider } from '@tanstack/react-query';

const queryClient = new QueryClient();

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <HashRouter>
      <QueryClientProvider client={queryClient}>
        <Content />
      </QueryClientProvider>
    </HashRouter>
  </React.StrictMode>
);

function Content() {
  return (
    <Routes>
      <Route path="/" element={<Navigate to="/runs" replace />} />
    </Routes>
  );
}
