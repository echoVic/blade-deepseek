import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL(".", import.meta.url));

export default defineConfig({
  plugins: [react()],
  base: "/",
  build: {
    rollupOptions: {
      input: {
        main: resolve(root, "index.html"),
        changelog: resolve(root, "changelog/index.html"),
        terminalCodingAgent: resolve(root, "terminal-coding-agent/index.html"),
        deepseekCodingAgent: resolve(root, "deepseek-coding-agent/index.html"),
        githubWorkflows: resolve(root, "github/index.html"),
        mcp: resolve(root, "mcp/index.html"),
      },
    },
  },
});
