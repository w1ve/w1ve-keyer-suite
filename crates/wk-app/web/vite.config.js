import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Fixed dev port (5176) so it does not clash with the Keyer-Router (5174), the Remote Keyer
// (5175), or the relay dashboard (5173). The build output is a plain relative bundle the
// webview loads from disk.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 5176, strictPort: true },
  build: { outDir: "dist", emptyOutDir: true, target: "es2021" },
});
