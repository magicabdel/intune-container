import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri desktop build: emit a self-contained static bundle that the Rust crate
// embeds at compile time (see `frontendDist` in ../tauri.conf.json).
export default defineConfig({
  plugins: [react({ jsxImportSource: "@emotion/react" })],
  // Tauri expects a fixed dev port and quiet output.
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    target: "es2021",
    outDir: "dist",
    emptyOutDir: true,
    // Desktop app: no need to split into many chunks.
    chunkSizeWarningLimit: 2000,
  },
});
