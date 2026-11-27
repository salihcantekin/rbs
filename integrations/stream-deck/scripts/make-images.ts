// Writes the default key images of both states of every action (manifest.json refers to them).
// Run: npx vite-node scripts/make-images.ts
import { writeFileSync } from "node:fs";
import { drawKey } from "../src/pictures/pictures.js";
import { type Kind, type ViewState, look } from "../src/view.js";

const dir = "com.rbs.streamdeck.sdPlugin/imgs/";
const ctx = { elapsed: { ms: 0, at: 0, running: false }, liveSince: 0, now: 0, titled: false };
const on: ViewState = {
	scenes: { scenes: [{ id: 1, name: "Scene" }], program: 1, preview: 1, master: { muted: true } },
	sources: [{ id: 1, name: "Source", item: 1, visible: false, muted: true }],
	record: { state: "recording" },
	stream: { state: "live", connected: true },
	vcam: { sending: true },
	replay: { armed: true },
};
const off: ViewState = { ...on, scenes: { scenes: [{ id: 1, name: "Scene" }], program: 2, master: { muted: false } }, sources: [{ id: 1, name: "Source", item: 1, visible: true, muted: false }], record: { state: "idle" }, stream: { state: "idle" }, vcam: { sending: false } };
const kinds: [Kind, Record<string, string>][] = [
	["scene", { scene: "1" }], ["record", {}], ["record-pause", {}], ["stream", {}], ["source-visible", { source: "1" }], ["mute", { source: "1" }], ["vcam", {}], ["studio", {}], ["transition", {}],
	["scene-step", {}], ["replay", {}], ["marker", {}], ["volume", {}],
];
for (const [kind, settings] of kinds) {
	writeFileSync(`${dir}${kind}-key.svg`, drawKey(look(kind, settings, off, ctx)));
	writeFileSync(`${dir}${kind}-key-1.svg`, drawKey(look(kind, settings, on, ctx)));
}
