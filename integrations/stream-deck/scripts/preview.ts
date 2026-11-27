// Writes an HTML sheet of every key picture state, for the owner's approval of the visuals.
// Run: npx vite-node scripts/preview.ts <out.html>
import { writeFileSync } from "node:fs";
import { markerKey, replayKey, sceneStepKey, vcamKey, pauseKey, linkKey, muteKey, recordKey, sceneKey, streamKey, visibleKey } from "../src/pictures/pictures.js";

const groups: [string, string[]][] = [
	["Scene", [sceneKey("Main camera", true, false), sceneKey("Chat", false, false), sceneKey("Old scene", false, true)]],
	["Record and pause", [recordKey("idle"), recordKey("recording", "12:34"), pauseKey("recording"), pauseKey("paused")]],
	["Stream", [streamKey("idle"), streamKey("connecting"), streamKey("live", false, "12:34"), streamKey("live", false, "12:34", false), streamKey("reconnecting")]],
	["Show / hide source", [visibleKey("Camera", true), visibleKey("Camera", false), visibleKey("Overlay", undefined)]],
	["Mute", [muteKey("Mic", false), muteKey("Mic", true), muteKey("Master", false)]],
	["Scene step, replay, camera, marker", [sceneStepKey("next", "Chat"), sceneStepKey("previous", "Main"), replayKey("armed"), replayKey("saving"), replayKey("off"), vcamKey("off"), vcamKey("on"), markerKey(true), markerKey(false)]],
	["Scene colours", [sceneKey("Orange", true, false, false, "#ff9f43"), sceneKey("Green", true, false, false, "#2ecc71"), sceneKey("Purple", true, false, false, "#a78bfa")]],
	["No link", [linkKey({ title: "Not connected" }), linkKey({ title: "Set token" }), linkKey({ title: "Token rejected" }), linkKey({ title: "Update RBS" })]],
];
const html = `<body style="margin:0;padding:16px;background:#0b0b0d;font:14px Segoe UI,sans-serif;color:#ddd;width:1500px">${groups
	.map(([n, s]) => `<div style="margin:0 0 14px"><div style="margin:0 0 6px">${n}</div><div style="display:flex;gap:10px">${s.join("")}</div></div>`)
	.join("")}</body>`;
writeFileSync(process.argv[2] ?? "preview.html", html);
