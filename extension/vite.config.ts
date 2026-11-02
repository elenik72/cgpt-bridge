import { defineConfig } from "vite";
import { resolve } from "node:path";

// We run vite once per ENTRY (background, content, pageShim). Each run
// emits a single self-contained IIFE bundle so the manifest can reference
// plain .js files. emptyOutDir is false so the runs accumulate into one
// dist/.
const ALLOWED_ENTRIES = ["background", "content", "pageShim"] as const;
type Entry = (typeof ALLOWED_ENTRIES)[number];
const entry: Entry = ALLOWED_ENTRIES.includes(
  process.env.ENTRY as Entry,
)
  ? (process.env.ENTRY as Entry)
  : "background";

const LIB_NAMES: Record<Entry, string> = {
  background: "cgptBridgeBackground",
  content: "cgptBridgeContent",
  pageShim: "cgptBridgePageShim",
};
const libName = LIB_NAMES[entry];

// The build id (the release scripts set the commit's short SHA), read by
// bridgeProtocol.ts as EXTENSION_BUILD; `dev` without it.
const build = process.env.CGPT_BUILD || "dev";

export default defineConfig({
  define: {
    __CGPT_BUILD__: JSON.stringify(build),
  },
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
    rolldownOptions: {
      output: {
        extend: true,
        // Keep the bundles strict. Rolldown's default "auto" keeps only source
        // directives (ours have none); with true it puts "use strict" at the
        // top of each bundle file, before the IIFE (Rollup put it inside).
        strict: true,
      },
    },
  },
});
