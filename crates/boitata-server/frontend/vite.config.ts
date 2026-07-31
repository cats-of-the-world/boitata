import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Built output is embedded into the server binary (see src/assets.rs), so assets
// are referenced relatively (`base: "./"`). In dev, proxy the API to the running
// `boitata-server` so the SPA and backend share an origin.
export default defineConfig({
  plugins: [react()],
  base: "./",
  build: {
    outDir: "dist",
    emptyOutDir: true,
  },
  server: {
    proxy: {
      "/api": "http://127.0.0.1:8787",
    },
  },
});
