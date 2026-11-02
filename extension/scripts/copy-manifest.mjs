import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

const root = process.cwd();
const src = resolve(root, "manifest.json");
const distDir = resolve(root, "dist");
const dest = resolve(distDir, "manifest.json");

// chrome://extensions shows `version_name` instead of `version`: a free-form
// string, so it can carry the build id (CGPT_BUILD, `dev` without it) that
// `version` (dot-separated numbers only) cannot.
const manifest = JSON.parse(readFileSync(src, "utf8"));
const build = process.env.CGPT_BUILD || "dev";
manifest.version_name = `${manifest.version} (${build})`;

mkdirSync(distDir, { recursive: true });
writeFileSync(dest, `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`copied ${src} -> ${dest} (version_name ${manifest.version_name})`);
