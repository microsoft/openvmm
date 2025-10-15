// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Custom plugin to add copyright header
const copyrightPlugin = () => ({
  name: 'copyright-header',
  generateBundle(options: any, bundle: any) {
    const header = '// Copyright (c) Microsoft Corporation.\n// Licensed under the MIT License.\n\n';
    Object.values(bundle).forEach((file: any) => {
      if (file.type === 'chunk' && file.code) {
        file.code = header + file.code;
      }
      if (file.type === 'asset' && file.source && file.fileName.endsWith('.css')) {
        file.source = `/* Copyright (c) Microsoft Corporation. Licensed under the MIT License. */\n\n` + file.source;
      }
    });
  }
});

// https://vitejs.dev/config/
export default defineConfig({
  base: "/test-results-new/",
  plugins: [react(), copyrightPlugin()],
  server: {
    port: 3000,
    open: true,
  }
});
