import { defineConfig } from "vite";
import { resolve } from "node:path";

// We run vite twice with ENTRY=background and ENTRY=content. Each run emits a
// single self-contained IIFE bundle so the manifest can reference plain .js
// files. emptyOutDir is false so the two runs accumulate into one dist/.
const entry =
  process.env.ENTRY === "content" ? "content" : "background";

const libName =
  entry === "background" ? "cgptBridgeBackground" : "cgptBridgeContent";

export default defineConfig({
  build: {
    outDir: "dist",
    emptyOutDir: false,
    target: "chrome120",
    minify: false,
    sourcemap: true,
    lib: {
      entry: resolve(process.cwd(), `src/${entry}.ts`),
      formats: ["iife"],
      name: libName,
      fileName: () => `${entry}.js`,
    },
    rollupOptions: {
      output: {
        extend: true,
      },
    },
  },
});
