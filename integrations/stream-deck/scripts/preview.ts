// Writes an HTML sheet of every key picture state, for the owner's approval of the visuals.
// Run: npx vite-node scripts/preview.ts <out.html>
import { writeFileSync } from "node:fs";
import { pauseKey, linkKey, muteKey, recordKey, sceneKey, streamKey, visibleKey } from "../src/pictures/pictures.js";

const groups: [string, string[]][] = [
	["Scene", [sceneKey("Main camera", true, false), sceneKey("Chat", false, false), sceneKey("Old scene", false, true)]],
	["Record and pause", [recordKey("idle"), recordKey("recording", "12:34"), pauseKey("recording"), pauseKey("paused")]],
	["Stream", [streamKey("idle"), streamKey("connecting"), streamKey("live"), streamKey("reconnecting")]],
	["Show / hide source", [visibleKey("Camera", true), visibleKey("Camera", false), visibleKey("Overlay", undefined)]],
	["Mute", [muteKey("Mic", false), muteKey("Mic", true), muteKey("Master", false)]],
	["No link", [linkKey({ title: "Not connected" }), linkKey({ title: "Set token" }), linkKey({ title: "Token rejected" }), linkKey({ title: "Update RBS" })]],
];
const html = `<body style="margin:0;padding:16px;background:#0b0b0d;font:14px Segoe UI,sans-serif;color:#ddd;width:760px">${groups
	.map(([n, s]) => `<div style="margin:0 0 14px"><div style="margin:0 0 6px">${n}</div><div style="display:flex;gap:10px">${s.join("")}</div></div>`)
	.join("")}</body>`;
writeFileSync(process.argv[2] ?? "preview.html", html);
