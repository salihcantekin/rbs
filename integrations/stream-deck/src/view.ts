import type { LinkStatus } from "./rbs-client/client.js";
import { type KeyLook, type LinkText, drawKey, glyphs, linkKey } from "./pictures/pictures.js";
import { tokens } from "./pictures/tokens.js";

/** Settings of each action (stored by Stream Deck; ids are strings from the inspector). */
export interface SceneSettings { scene?: string; sceneName?: string; color?: string; direction?: "next" | "previous"; step?: string }
export interface SourceSettings { source?: string; sourceName?: string }
/** A library entry, a filter or a bus a key points at. */
export interface TargetSettings { entry?: string; filter?: string; bus?: string }
/** What a press does on a two-state key: both ways, only the first (start, show, mute, pause), or only the second. */
export type Mode = "toggle" | "on" | "off";
export interface ModeSettings { mode?: Mode; label0?: string; label1?: string }
export type Settings = SceneSettings & SourceSettings & ModeSettings & TargetSettings;
export type Kind = "scene" | "record" | "record-pause" | "scene-step" | "replay" | "vcam" | "marker" | "volume" | "studio" | "transition" | "stream" | "source-visible" | "mute" | "replay-buffer" | "collection" | "profile" | "preview" | "status" | "go-live" | "filter" | "bus-mute" | "bus-volume";

/** The parts of RBS's state the keys read, loosely typed (the server may add fields). */
export interface ViewState {
	scenes?: { scenes: { id: number; name: string; active?: boolean }[]; program: number; preview?: number; master?: { muted?: boolean; volume?: number }; sources?: { id: number; name: string }[] };
	sources?: { id: number; name: string; item?: number; visible?: boolean; muted?: boolean; volume?: number; filters?: { id: number; type: string; enabled: boolean }[] }[];
	buses?: { buses: { name: string; volume: number; muted: boolean }[] };
	collections?: { entries: { id: string; name: string }[]; current: string };
	profileLibrary?: { entries: { id: string; name: string }[]; current: string };
	record?: { state: string; paused?: boolean };
	stream?: { state: string; connected?: boolean; connects?: number };
	replay?: { armed?: boolean; saving?: boolean };
	vcam?: { sending?: boolean };
}

/** Recording time as the key shows it: the last known time plus time since the event. */
export interface Elapsed { ms: number; at: number; running: boolean }

export function linkText(s: LinkStatus): LinkText {
	switch (s.state) {
		case "idle": return { title: "Not connected" };
		case "no_token": return { title: "Set token" };
		case "unreachable": return { title: "Not connected" };
		case "auth_failed": return { title: "Token rejected" };
		case "revoked": return { title: "Token revoked" };
		case "incompatible": return { title: s.detail === "server_too_old" ? "Update RBS" : "Update plugin" };
		default: return { title: "Connecting" };
	}
}

/** A filter's type id (`org.rbs.filter.color-correction`) as words: "Color correction". */
export function filterName(type: string): string {
	const last = type.slice(type.lastIndexOf(".") + 1).replace(/[-_]+/g, " ").trim();
	return last ? last[0]!.toUpperCase() + last.slice(1) : type;
}

/** The filter a key points at, with the name of its source. */
export function findFilter(state: ViewState, id: string | undefined): { source: string; name: string; type: string; enabled: boolean; id: number } | undefined {
	for (const src of state.sources ?? []) {
		const f = src.filters?.find((x) => String(x.id) === id);
		if (f) return { source: src.name, name: f.type, type: f.type, enabled: f.enabled, id: f.id };
	}
	return undefined;
}

const bus = (state: ViewState, name: string | undefined) => state.buses?.buses.find((b) => b.name === name);

/** What a volume dial shows and acts on. */
export function dialView(kind: Kind, settings: Settings, state: ViewState): { title: string; volume?: number; muted?: boolean } {
	if (kind === "bus-volume") {
		const b = bus(state, settings.bus);
		return { title: settings.bus ?? "Track", volume: b?.volume, muted: b?.muted === true };
	}
	if (settings.source === "master") {
		const m = state.scenes?.master;
		return { title: "Master", volume: state.scenes ? (m?.volume ?? 1) : undefined, muted: m?.muted === true };
	}
	const src = state.sources?.find((s) => String(s.id) === settings.source);
	const name = src?.name ?? settings.sourceName ?? "";
	return { title: name, volume: src ? (src.volume ?? 1) : undefined, muted: src?.muted === true };
}

export function formatElapsed(ms: number): string {
	const total = Math.max(0, Math.floor(ms / 1000));
	const h = Math.floor(total / 3600);
	const m = Math.floor((total % 3600) / 60);
	const s = total % 60;
	const two = (n: number) => String(n).padStart(2, "0");
	return h > 0 ? `${h}:${two(m)}:${two(s)}` : `${two(m)}:${two(s)}`;
}

/** The SVG of one key. Pure: everything it needs is passed in. */
export interface RenderContext {
	elapsed: Elapsed;
	/** When the stream went live (ms, 0 when unknown). */
	liveSince: number;
	now: number;
	/** The user set a Stream Deck title: draw no text. */
	titled: boolean;
}

/** The scene a step key goes to: the next or previous one that navigation reaches, wrapping around. */
export function stepTarget(state: ViewState, direction: "next" | "previous"): { id: number; name: string } | undefined {
	const scenes = (state.scenes?.scenes ?? []).filter((s) => s.active !== false);
	// Studio mode steps the preview scene; otherwise the program.
	const from = state.scenes?.preview ?? state.scenes?.program;
	const at = scenes.findIndex((s) => s.id === from);
	if (scenes.length < 2) return undefined;
	const i = at < 0 ? 0 : (at + (direction === "next" ? 1 : -1) + scenes.length) % scenes.length;
	return scenes[i];
}

const live = (g: string) => g.replace(/currentColor/g, tokens.live);
const validColor = (c: string | undefined) => (/^#[0-9a-f]{6}$/i.test(c ?? "") ? c : undefined);
export const modeOf = (s: Settings): Mode => (s.mode === "on" || s.mode === "off" ? s.mode : "toggle");

/** The key's state index (the manifest's two states): what the user can give a title of its own. */
export function stateIndex(kind: Kind, settings: Settings, state: ViewState): 0 | 1 {
	switch (kind) {
		case "scene": return state.scenes?.program === Number(settings.scene) ? 1 : 0;
		case "record": return state.record?.state === "recording" ? 1 : 0;
		case "record-pause": return state.record?.state === "recording" && state.record.paused ? 1 : 0;
		case "stream": return state.stream?.state === "live" ? 1 : 0;
		case "source-visible": {
			const src = state.sources?.find((x) => String(x.id) === settings.source);
			return src && src.item !== undefined && src.visible === false ? 1 : 0;
		}
		case "mute": return (settings.source === "master" ? state.scenes?.master?.muted : state.sources?.find((x) => String(x.id) === settings.source)?.muted) ? 1 : 0;
		case "vcam": return state.vcam?.sending ? 1 : 0;
		case "studio": return state.scenes?.preview !== undefined ? 1 : 0;
		case "replay-buffer": return state.replay?.armed ? 1 : 0;
		case "collection": return state.collections?.current === settings.entry ? 1 : 0;
		case "profile": return state.profileLibrary?.current === settings.entry ? 1 : 0;
		case "go-live": return state.record?.state === "recording" || state.stream?.state === "live" ? 1 : 0;
		case "filter": return findFilter(state, settings.filter)?.enabled === false ? 1 : 0;
		case "bus-mute": return bus(state, settings.bus)?.muted ? 1 : 0;
		default: return 0;
	}
}

/** The look of one key. Pure: everything it needs is passed in. */
const LARGE = new Set<string>([glyphs.recordIdle, live(glyphs.recordIdle), glyphs.stop, glyphs.pause, glyphs.play]);

/** The look of one key, with its icon size class. */
export function look(kind: Kind, settings: Settings, state: ViewState, ctx: RenderContext): KeyLook {
	let k = lookRaw(kind, settings, state, ctx);
	const mode = modeOf(settings);
	// The user's own text replaces ours (Stream Deck would draw its Title in every state). A key with one job
	// (start only, stop only, ...) has one text; a key that does both jobs has one per state.
	const own = (mode === "toggle" && stateIndex(kind, settings, state) === 1 ? settings.label1 : settings.label0)?.trim();
	if (own && !ctx.titled) k = { ...k, label: own };
	// A one-job key whose job is done is passive: dim, no ring, no pulse.
	if (mode !== "toggle" && jobDone(kind, settings, state, mode)) k = { ...k, tone: "dim", ring: false, pulse: false, dot: false };
	return k.size ? k : { ...k, size: LARGE.has(k.glyph) ? "large" : k.glyph === glyphs.warn ? "small" : "medium" };
}

/** True when a one-job key (mode "on" or "off") has nothing left to do: pressing it would change nothing. */
export function jobDone(kind: Kind, settings: Settings, state: ViewState, mode: "on" | "off"): boolean {
	const on = mode === "on";
	const src = state.sources?.find((x) => String(x.id) === settings.source);
	switch (kind) {
		case "record": return (state.record?.state === "recording") === on;
		case "record-pause": return (state.record?.state === "recording" && state.record.paused === true) === on;
		case "stream": return (state.stream?.state === "live") === on;
		case "source-visible": return src?.item !== undefined && (src.visible !== false) === on;
		case "mute": {
			const muted = settings.source === "master" ? state.scenes?.master?.muted : src?.muted;
			return muted !== undefined && (muted === true) === on;
		}
		case "vcam": return (state.vcam?.sending === true) === on;
		case "studio": return (state.scenes?.preview !== undefined) === on;
		case "replay-buffer": return (state.replay?.armed === true) === on;
		case "go-live": return (state.record?.state === "recording" || state.stream?.state === "live") === on;
		case "filter": {
			const f = findFilter(state, settings.filter);
			return f !== undefined && f.enabled === on;
		}
		case "bus-mute": {
			const b = bus(state, settings.bus);
			return b !== undefined && b.muted === (on === true);
		}
		default: return false;
	}
}

function lookRaw(kind: Kind, settings: Settings, state: ViewState, ctx: RenderContext): KeyLook {
	const { elapsed, now } = ctx;
	const pulse = Math.floor(now / 1000) % 2 === 0;
	const mode = modeOf(settings);
	const untitled = ctx.titled;
	switch (kind) {
		case "scene": {
			const id = Number(settings.scene);
			const scene = state.scenes?.scenes.find((x) => x.id === id);
			if (!scene) return { glyph: glyphs.scene, label: "Missing scene", tone: "dim", untitled };
			const active = state.scenes?.program === id;
			return { glyph: glyphs.scene, label: scene.name, tone: active ? "accent" : "neutral", accent: validColor(settings.color), dot: active, untitled };
		}
		case "record": {
			const r = state.record;
			const recording = r?.state === "recording";
			const time = formatElapsed(elapsed.ms + (elapsed.running ? now - elapsed.at : 0));
			if (!recording) return mode === "off" ? { glyph: glyphs.stop, label: "Stop", tone: "dim", untitled } : { glyph: live(glyphs.recordIdle), label: "Record", tone: "neutral", untitled };
			if (mode === "on") return { glyph: live(glyphs.recordIdle), label: "Recording", sub: time, tone: "live", ring: true, pulse, untitled };
			return { glyph: glyphs.stop, label: r?.paused ? "Stop (paused)" : "Stop", sub: time, tone: "live", ring: !r?.paused, pulse, untitled };
		}
		case "record-pause": {
			const r = state.record;
			const recording = r?.state === "recording";
			const paused = recording && r.paused === true;
			if (paused) return mode === "on" ? { glyph: glyphs.pause, label: "Paused", tone: "warn", untitled } : { glyph: glyphs.play, label: "Resume", tone: "go", untitled };
			if (mode === "off") return { glyph: glyphs.play, label: "Resume", tone: "dim", untitled };
			return { glyph: glyphs.pause, label: "Pause", tone: recording ? "neutral" : "dim", untitled };
		}
		case "stream": {
			const s = state.stream;
			const on = s?.state === "live";
			if (!on) return mode === "off" ? { glyph: glyphs.stream, label: "Stop", tone: "dim", untitled } : { glyph: glyphs.stream, label: "Stream", tone: "neutral", untitled };
			const status = s.connected ? (ctx.liveSince > 0 ? formatElapsed(now - ctx.liveSince) : "Live") : (s.connects ?? 0) > 0 ? "Reconnecting" : "Connecting";
			return { glyph: glyphs.stream, label: mode === "on" ? "Live" : "Stop", sub: status, tone: "live", ring: true, pulse, untitled };
		}
		case "source-visible": {
			const src = state.sources?.find((x) => String(x.id) === settings.source);
			const name = src?.name ?? settings.sourceName ?? "Source";
			if (!src || src.item === undefined) return { glyph: glyphs.eyeShut, label: name, sub: "Not in scene", tone: "dim", untitled };
			const visible = src.visible !== false;
			if (mode === "on") return { glyph: glyphs.eyeOpen, label: name, sub: "Show", tone: visible ? "accent" : "neutral", untitled };
			if (mode === "off") return { glyph: glyphs.eyeShut, label: name, sub: "Hide", tone: visible ? "neutral" : "accent", untitled };
			return { glyph: visible ? glyphs.eyeOpen : glyphs.eyeShut, label: name, tone: visible ? "neutral" : "dim", untitled };
		}
		case "mute": {
			const master = settings.source === "master";
			const src = state.sources?.find((x) => String(x.id) === settings.source);
			const name = master ? "Master" : (src?.name ?? settings.sourceName ?? "Mute");
			const muted = master ? (state.scenes ? state.scenes.master?.muted === true : undefined) : src ? src.muted === true : undefined;
			if (mode === "on") return { glyph: glyphs.speakerOff, label: name, sub: "Mute", tone: muted ? "accent" : "neutral", untitled };
			if (mode === "off") return { glyph: glyphs.speakerOn, label: name, sub: "Unmute", tone: muted === false ? "accent" : "neutral", untitled };
			if (muted === undefined) return { glyph: glyphs.speaker, label: name, tone: "dim", untitled };
			return { glyph: muted ? glyphs.speakerOff : glyphs.speakerOn, label: name, tone: muted ? "dim" : "neutral", untitled };
		}
		case "vcam": {
			const on = state.vcam?.sending === true;
			if (mode === "on") return { glyph: glyphs.camera, label: on ? "Camera on" : "Start cam", tone: on ? "accent" : "neutral", dot: on, untitled };
			if (mode === "off") return { glyph: glyphs.camera, label: "Stop cam", tone: on ? "neutral" : "dim", untitled };
			return { glyph: glyphs.camera, label: on ? "Camera on" : "Virtual cam", tone: on ? "accent" : "neutral", dot: on, untitled };
		}
		case "studio": {
			const on = state.scenes?.preview !== undefined;
			if (mode === "off") return { glyph: glyphs.studio, label: "Studio off", tone: on ? "neutral" : "dim", untitled };
			return { glyph: glyphs.studio, label: on ? "Studio on" : "Studio mode", tone: on ? "accent" : "neutral", dot: on, untitled };
		}
		case "transition":
			return { glyph: glyphs.transition, label: "Transition", tone: state.scenes?.preview !== undefined ? "neutral" : "dim", untitled };
		case "scene-step": {
			const dir = settings.direction === "previous" ? "previous" : "next";
			const target = stepTarget(state, dir)?.name;
			return { glyph: glyphs[dir], label: dir === "next" ? "Next scene" : "Previous", sub: target, tone: target ? "neutral" : "dim", untitled };
		}
		case "replay": {
			const r = state.replay;
			if (r?.saving) return { glyph: glyphs.replay, label: "Saving", tone: "accent", ring: true, pulse, untitled };
			return { glyph: glyphs.replay, label: r?.armed ? "Save replay" : "Replay off", tone: r?.armed ? "neutral" : "dim", untitled };
		}
		case "marker":
			return { glyph: glyphs.marker, label: "Marker", tone: state.record?.state === "recording" ? "neutral" : "dim", untitled };
		case "volume":
			return { glyph: glyphs.volume, label: "Volume", tone: "neutral", untitled };
		case "bus-volume":
			return { glyph: glyphs.bus, label: settings.bus ?? "Track", tone: "neutral", untitled };
		case "replay-buffer": {
			const on = state.replay?.armed === true;
			if (mode === "on") return { glyph: glyphs.replay, label: on ? "Replay on" : "Start replay", tone: on ? "accent" : "neutral", dot: on, untitled };
			if (mode === "off") return { glyph: glyphs.replay, label: "Stop replay", tone: on ? "neutral" : "dim", untitled };
			return { glyph: glyphs.replay, label: on ? "Replay on" : "Replay off", tone: on ? "accent" : "neutral", dot: on, untitled };
		}
		case "collection":
		case "profile": {
			const lib = kind === "collection" ? state.collections : state.profileLibrary;
			const entry = lib?.entries.find((e) => e.id === settings.entry);
			const word = kind === "collection" ? "Collection" : "Profile";
			if (!settings.entry) return { glyph: kind === "collection" ? glyphs.library : glyphs.profile, label: word, sub: "Choose one", tone: "dim", untitled };
			if (!entry) return { glyph: kind === "collection" ? glyphs.library : glyphs.profile, label: settings.sourceName ?? word, sub: "Not found", tone: "dim", untitled };
			const active = lib?.current === entry.id;
			return { glyph: kind === "collection" ? glyphs.library : glyphs.profile, label: entry.name, sub: active ? "In use" : word, tone: active ? "accent" : "neutral", dot: active, untitled };
		}
		case "preview": {
			const scene = state.scenes?.scenes.find((x) => x.id === Number(settings.scene));
			if (!scene) return { glyph: glyphs.preview, label: "Choose a scene", tone: "dim", untitled };
			const studio = state.scenes?.preview !== undefined;
			const on = state.scenes?.preview === scene.id;
			return { glyph: glyphs.preview, label: scene.name, sub: studio ? "Preview" : "Studio off", tone: !studio ? "dim" : on ? "accent" : "neutral", dot: on, untitled };
		}
		case "status": {
			const rec = state.record?.state === "recording";
			const on = state.stream?.state === "live";
			const time = formatElapsed(elapsed.ms + (elapsed.running ? now - elapsed.at : 0));
			const streamText = on ? (state.stream?.connected ? (ctx.liveSince > 0 ? formatElapsed(now - ctx.liveSince) : "Live") : "Connecting") : "";
			if (rec && on) return { glyph: live(glyphs.recordIdle), label: "Rec + Live", sub: time, tone: "live", ring: !state.record?.paused, pulse, untitled };
			if (rec) return { glyph: live(glyphs.recordIdle), label: state.record?.paused ? "Paused" : "Recording", sub: time, tone: "live", ring: !state.record?.paused, pulse, untitled };
			if (on) return { glyph: glyphs.stream, label: "Live", sub: streamText, tone: "live", ring: true, pulse, untitled };
			return { glyph: glyphs.info, label: "Idle", tone: "dim", untitled };
		}
		case "go-live": {
			const running = state.record?.state === "recording" || state.stream?.state === "live";
			if (mode === "on") return { glyph: glyphs.goLive, label: running ? "Running" : "Go live", tone: running ? "live" : "neutral", ring: running, pulse, untitled };
			if (mode === "off") return { glyph: glyphs.stop, label: "End all", tone: running ? "live" : "dim", untitled };
			return { glyph: running ? glyphs.stop : glyphs.goLive, label: running ? "End all" : "Go live", tone: running ? "live" : "neutral", ring: running, pulse, untitled };
		}
		case "filter": {
			const f = findFilter(state, settings.filter);
			if (!settings.filter) return { glyph: glyphs.filter, label: "Filter", sub: "Choose one", tone: "dim", untitled };
			if (!f) return { glyph: glyphs.filter, label: "Filter", sub: "Not in scene", tone: "dim", untitled };
			const name = filterName(f.type);
			if (mode === "on") return { glyph: glyphs.filter, label: name, sub: "Turn on", tone: f.enabled ? "accent" : "neutral", untitled };
			if (mode === "off") return { glyph: glyphs.filter, label: name, sub: "Turn off", tone: f.enabled ? "neutral" : "accent", untitled };
			return { glyph: glyphs.filter, label: name, sub: name.includes(" ") ? undefined : f.source, tone: f.enabled ? "accent" : "dim", dot: f.enabled, untitled };
		}
		case "bus-mute": {
			const b = bus(state, settings.bus);
			if (!settings.bus) return { glyph: glyphs.bus, label: "Track", sub: "Choose one", tone: "dim", untitled };
			if (!b) return { glyph: glyphs.bus, label: settings.bus, sub: "Not found", tone: "dim", untitled };
			if (mode === "on") return { glyph: glyphs.speakerOff, label: b.name, sub: "Mute", tone: b.muted ? "accent" : "neutral", untitled };
			if (mode === "off") return { glyph: glyphs.speakerOn, label: b.name, sub: "Unmute", tone: b.muted ? "neutral" : "accent", untitled };
			return { glyph: b.muted ? glyphs.speakerOff : glyphs.speakerOn, label: b.name, tone: b.muted ? "dim" : "neutral", untitled };
		}
	}
}

/**
 * Link states in which keys keep their usual look: RBS is closed, remote control is off or the link
 * is coming up. A press then shows the link state for a moment (`NOTICE_MS`). The other states need
 * the user to act (token, version), so every key keeps showing them.
 */
export function keepsLook(link: LinkStatus): boolean {
	return link.state === "idle" || link.state === "connecting" || link.state === "unreachable";
}

/** How long a press while RBS is not reachable shows the link state on the key, in ms. */
export const NOTICE_MS = 2000;

/**
 * The look of a key while RBS is not reachable: what the key does, with the names it saved
 * (`rememberedName`), and no live state (no tone, ring, dot or second line).
 */
export function restingLook(kind: Kind, settings: Settings, ctx: RenderContext): KeyLook {
	const k = look(kind, settings, {}, ctx);
	const saved = kind === "scene" || kind === "preview" ? settings.sceneName : settings.sourceName;
	const fallback: Partial<Record<Kind, string>> = {
		scene: "Scene",
		preview: "Preview",
		"source-visible": "Source",
		mute: settings.source === "master" ? "Master" : "Mute",
		filter: "Filter",
		"bus-mute": settings.bus ?? "Track",
		collection: "Collection",
		profile: "Profile",
	};
	const own = settings.label0?.trim();
	const label = own && !ctx.titled ? own : (fallback[kind] !== undefined ? (saved?.trim() || fallback[kind]!) : k.label);
	return { ...k, label, sub: undefined, tone: "neutral", ring: false, pulse: false, dot: false };
}

/**
 * The name a key saves in its settings while RBS is reachable, so it can show it while RBS is not:
 * the scene, source, collection or profile it points at. Undefined when there is nothing new to save.
 */
export function rememberedName(kind: Kind, settings: Settings, state: ViewState): { sceneName: string } | { sourceName: string } | undefined {
	if (kind === "scene" || kind === "preview") {
		const name = state.scenes?.scenes.find((x) => String(x.id) === settings.scene)?.name;
		return name && name !== settings.sceneName ? { sceneName: name } : undefined;
	}
	let name: string | undefined;
	if (kind === "source-visible" || ((kind === "mute" || kind === "volume") && settings.source !== "master")) {
		name = (state.scenes?.sources ?? state.sources)?.find((x) => String(x.id) === settings.source)?.name;
	} else if (kind === "collection" || kind === "profile") {
		name = (kind === "collection" ? state.collections : state.profileLibrary)?.entries.find((e) => e.id === settings.entry)?.name;
	}
	return name && name !== settings.sourceName ? { sourceName: name } : undefined;
}

/**
 * The SVG of one key. `notice` is true for a moment after a press while RBS is not reachable:
 * the key then shows the link state instead of its usual look.
 */
export function renderKey(kind: Kind, settings: Settings, link: LinkStatus, state: ViewState, ctx: RenderContext, notice = false): string {
	if (link.state === "ready") return drawKey(look(kind, settings, state, ctx));
	if (keepsLook(link) && !notice) return drawKey(restingLook(kind, settings, ctx));
	return linkKey(linkText(link));
}
