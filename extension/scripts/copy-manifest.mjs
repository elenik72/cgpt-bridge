import { copyFileSync, mkdirSync } from "node:fs";
import { resolve } from "node:path";

const root = process.cwd();
const src = resolve(root, "manifest.json");
const distDir = resolve(root, "dist");
const dest = resolve(distDir, "manifest.json");

mkdirSync(distDir, { recursive: true });
copyFileSync(src, dest);
console.log(`copied ${src} -> ${dest}`);
