import type { LinkStatus } from "./rbs-client/client.js";
import { type KeyLook, type LinkText, drawKey, glyphs, linkKey } from "./pictures/pictures.js";
import { tokens } from "./pictures/tokens.js";

/** Settings of each action (stored by Stream Deck; ids are strings from the inspector). */
export interface SceneSettings { scene?: string; sceneName?: string; color?: string; direction?: "next" | "previous"; step?: string }
export interface SourceSettings { source?: string; sourceName?: string }
/** What a press does on a two-state key: both ways, only the first (start, show, mute, pause), or only the second. */
export type Mode = "toggle" | "on" | "off";
export interface ModeSettings { mode?: Mode; label0?: string; label1?: string }
export type Settings = SceneSettings & SourceSettings & ModeSettings;
export type Kind = "scene" | "record" | "record-pause" | "scene-step" | "replay" | "vcam" | "marker" | "volume" | "studio" | "transition" | "stream" | "source-visible" | "mute";

/** The parts of RBS's state the keys read, loosely typed (the server may add fields). */
export interface ViewState {
	scenes?: { scenes: { id: number; name: string; active?: boolean }[]; program: number; preview?: number; master?: { muted?: boolean; volume?: number }; sources?: { id: number; name: string }[] };
	sources?: { id: number; name: string; item?: number; visible?: boolean; muted?: boolean; volume?: number }[];
	record?: { state: string; paused?: boolean };
	stream?: { state: string; connected?: boolean; connects?: number };
	replay?: { armed?: boolean; saving?: boolean };
	vcam?: { sending?: boolean };
}

/** Recording time as the key shows it: the last known time plus time since the event. */
export interface Elapsed { ms: number; at: number; running: boolean }

export function linkText(s: LinkStatus): LinkText {
	switch (s.state) {
		case "no_token": return { title: "Set token" };
		case "unreachable": return { title: "Not connected" };
		case "auth_failed": return { title: "Token rejected" };
		case "revoked": return { title: "Token revoked" };
		case "incompatible": return { title: s.detail === "server_too_old" ? "Update RBS" : "Update plugin" };
		default: return { title: "Connecting" };
	}
}

/** What a volume dial shows and acts on. */
export function dialView(settings: Settings, state: ViewState): { title: string; volume?: number; muted?: boolean } {
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
		default: return 0;
	}
}

/** The look of one key. Pure: everything it needs is passed in. */
const LARGE = new Set<string>([glyphs.recordIdle, live(glyphs.recordIdle), glyphs.stop, glyphs.pause, glyphs.play]);

/** The look of one key, with its icon size class. */
export function look(kind: Kind, settings: Settings, state: ViewState, ctx: RenderContext): KeyLook {
	const raw = lookRaw(kind, settings, state, ctx);
	// The user's own text for this state replaces ours (Stream Deck would draw its Title in every state).
	const own = (stateIndex(kind, settings, state) === 1 ? settings.label1 : settings.label0)?.trim();
	const k = own && !ctx.titled ? { ...raw, label: own } : raw;
	return k.size ? k : { ...k, size: LARGE.has(k.glyph) ? "large" : k.glyph === glyphs.warn ? "small" : "medium" };
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
	}
}

/** The SVG of one key. */
export function renderKey(kind: Kind, settings: Settings, link: LinkStatus, state: ViewState, ctx: RenderContext): string {
	if (link.state !== "ready") return linkKey(linkText(link));
	return drawKey(look(kind, settings, state, ctx));
}
