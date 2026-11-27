// Writes the default key images of both states of every action (manifest.json refers to them).
// Run: npx vite-node scripts/make-images.ts
import { writeFileSync } from "node:fs";
import { drawKey, glyphs } from "../src/pictures/pictures.js";
import { type Kind, type ViewState, look } from "../src/view.js";

const dir = "com.rbs.streamdeck.sdPlugin/imgs/";
const ctx = { elapsed: { ms: 0, at: 0, running: false }, liveSince: 0, now: 0, titled: false };
const on: ViewState = {
	scenes: { scenes: [{ id: 1, name: "Scene" }], program: 1, preview: 1, master: { muted: true } },
	sources: [{ id: 1, name: "Source", item: 1, visible: false, muted: true, filters: [{ id: 1, type: "Filter", enabled: false }] }],
	record: { state: "recording" },
	stream: { state: "live", connected: true },
	vcam: { sending: true },
	replay: { armed: true },
	buses: { buses: [{ name: "Music", volume: 1, muted: true }] },
	collections: { entries: [{ id: "a", name: "Collection" }], current: "a" },
	profileLibrary: { entries: [{ id: "a", name: "Profile" }], current: "a" },
};
const off: ViewState = { ...on, scenes: { scenes: [{ id: 1, name: "Scene" }], program: 2, master: { muted: false } }, sources: [{ id: 1, name: "Source", item: 1, visible: true, muted: false, filters: [{ id: 1, type: "Filter", enabled: true }] }], record: { state: "idle" }, stream: { state: "idle" }, vcam: { sending: false }, replay: { armed: false }, buses: { buses: [{ name: "Music", volume: 1, muted: false }] }, collections: { entries: [{ id: "a", name: "Collection" }], current: "b" }, profileLibrary: { entries: [{ id: "a", name: "Profile" }], current: "b" } };
const kinds: [Kind, Record<string, string>][] = [
	["scene", { scene: "1" }], ["record", {}], ["record-pause", {}], ["stream", {}], ["source-visible", { source: "1" }], ["mute", { source: "1" }], ["vcam", {}], ["studio", {}], ["transition", {}],
	["scene-step", {}], ["replay", {}], ["marker", {}], ["volume", {}],
	["replay-buffer", {}], ["collection", { entry: "a" }], ["profile", { entry: "a" }], ["preview", { scene: "1" }], ["status", {}], ["go-live", {}], ["filter", { filter: "1" }], ["bus-mute", { bus: "Music" }], ["bus-volume", { bus: "Music" }],
];
for (const [kind, settings] of kinds) {
	writeFileSync(`${dir}${kind}-key.svg`, drawKey(look(kind, settings, off, ctx)));
	writeFileSync(`${dir}${kind}-key-1.svg`, drawKey(look(kind, settings, on, ctx)));
}

// The small action-list icons of the actions added after 0.2 (white strokes in a 20 by 20 box).
const list: Record<string, string> = {
	replay: glyphs.replay, "replay-buffer": glyphs.replay, collection: glyphs.library, profile: glyphs.profile, preview: glyphs.preview, status: glyphs.info,
	"go-live": glyphs.goLive, filter: glyphs.filter, "bus-mute": glyphs.bus, "bus-volume": glyphs.bus,
};
for (const [name, glyph] of Object.entries(list)) {
	writeFileSync(`${dir}${name}.svg`, `<svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 64 64" fill="none" stroke="#fff" stroke-width="5" stroke-linecap="round" stroke-linejoin="round" color="#fff">${glyph}</svg>`);
}
