// Packs the .sdPlugin folder into dist/com.rbs.streamdeck.streamDeckPlugin with the Stream Deck CLI.
import { mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";

mkdirSync("dist", { recursive: true });
const r = spawnSync("npx", ["streamdeck", "pack", "com.rbs.streamdeck.sdPlugin", "--output", "dist", "--force"], { stdio: "inherit", shell: true });
process.exit(r.status ?? 1);
