// Writes an HTML sheet of the key pictures, for the owner's approval of the visuals.
// Run: npx vite-node scripts/preview.ts <out.html>
import { writeFileSync } from "node:fs";
import { drawKey, linkKey } from "../src/pictures/pictures.js";
import { type Kind, type Settings, type ViewState, look } from "../src/view.js";

const ctx = { elapsed: { ms: 754_000, at: 0, running: false }, liveSince: 0, now: 0, titled: false };
const st: ViewState = {
	scenes: { scenes: [{ id: 1, name: "Main camera" }, { id: 2, name: "Chat" }], program: 1, master: { muted: false, volume: 0.8 } },
	sources: [{ id: 7, name: "Camera", item: 1, visible: true, muted: false }, { id: 8, name: "Overlay", item: 2, visible: false, muted: true }, { id: 9, name: "Mic", muted: false }],
	record: { state: "idle" }, stream: { state: "idle" }, vcam: { sending: false }, replay: { armed: true },
};
const rec: ViewState = { ...st, record: { state: "recording" }, stream: { state: "live", connected: true, connects: 1 } };
const paused: ViewState = { ...st, record: { state: "recording", paused: true } };
const k = (kind: Kind, s: ViewState = st, settings: Settings = {}, c = ctx) => drawKey(look(kind, settings, s, c));
const groups: [string, string[]][] = [
	["Scene", [k("scene", st, { scene: "1" }), k("scene", st, { scene: "2" }), k("scene", st, { scene: "1", color: "#ff9f43" }), k("scene", st, { scene: "9" })]],
	["Record / pause", [k("record"), k("record", rec), k("record", rec, {}, { ...ctx, now: 1000 }), k("record-pause", st), k("record-pause", rec), k("record-pause", paused)]],
	["Stream", [k("stream"), k("stream", rec), k("stream", { ...rec, stream: { state: "live", connected: false, connects: 2 } })]],
	["Source", [k("source-visible", st, { source: "7" }), k("source-visible", st, { source: "8" }), k("source-visible", st, { source: "9" }), k("mute", st, { source: "7" }), k("mute", st, { source: "8" }), k("mute", st, { source: "master" })]],
	["Modes", [k("record", st, { mode: "on" }), k("record", rec, { mode: "on" }), k("record", st, { mode: "off" }), k("record", rec, { mode: "off" }), k("source-visible", st, { source: "7", mode: "on" }), k("mute", st, { source: "7", mode: "off" })]],
	["More", [k("scene-step", st, { direction: "next" }), k("scene-step", st, { direction: "previous" }), k("replay"), k("replay", { ...st, replay: { armed: true, saving: true } }), k("vcam"), k("vcam", { ...st, vcam: { sending: true } }), k("marker"), k("marker", rec), k("studio"), k("studio", { ...st, scenes: { ...st.scenes!, preview: 2 } }), k("transition", { ...st, scenes: { ...st.scenes!, preview: 2 } })]],
	["No link", ["Not connected", "Set token", "Token rejected", "Update RBS"].map((title) => linkKey({ title }))],
	["With your own title (no text)", [k("record", rec, {}, { ...ctx, titled: true }), k("scene", st, { scene: "1" }, { ...ctx, titled: true }), k("mute", st, { source: "7" }, { ...ctx, titled: true })]],
];
const html = `<body style="margin:0;padding:16px;background:#0b0b0d;font:14px Segoe UI,sans-serif;color:#ddd;width:1100px">${groups
	.map(([n, s]) => `<div style="margin:0 0 14px"><div style="margin:0 0 6px">${n}</div><div style="display:flex;flex-wrap:wrap;gap:10px">${s.join("")}</div></div>`)
	.join("")}</body>`;
writeFileSync(process.argv[2] ?? "preview.html", html);
