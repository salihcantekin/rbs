import type { LinkStatus } from "./rbs-client/client.js";
import { type LinkText, linkKey, markerKey, muteKey, pauseKey, recordKey, replayKey, sceneKey, sceneStepKey, streamKey, vcamKey, visibleKey } from "./pictures/pictures.js";

/** Settings of each action (stored by Stream Deck; ids are strings from the inspector). */
export interface SceneSettings { scene?: string; sceneName?: string; color?: string; direction?: "next" | "previous"; step?: string }
export interface SourceSettings { source?: string; sourceName?: string }
export type Settings = SceneSettings & SourceSettings;
export type Kind = "scene" | "record" | "record-pause" | "scene-step" | "replay" | "vcam" | "marker" | "volume" | "stream" | "source-visible" | "mute";

/** The parts of RBS's state the keys read, loosely typed (the server may add fields). */
export interface ViewState {
	scenes?: { scenes: { id: number; name: string; active?: boolean }[]; program: number; master?: { muted?: boolean; volume?: number }; sources?: { id: number; name: string }[] };
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
	const at = scenes.findIndex((s) => s.id === state.scenes?.program);
	if (scenes.length < 2) return undefined;
	const i = at < 0 ? 0 : (at + (direction === "next" ? 1 : -1) + scenes.length) % scenes.length;
	return scenes[i];
}

export function renderKey(kind: Kind, settings: Settings, link: LinkStatus, state: ViewState, ctx: RenderContext): string {
	const { elapsed, now, titled } = ctx;
	const pulse = Math.floor(now / 1000) % 2 === 0;
	if (link.state !== "ready") return linkKey(linkText(link));
	switch (kind) {
		case "scene": {
			const id = Number(settings.scene);
			const scene = state.scenes?.scenes.find((s) => s.id === id);
			return sceneKey(scene?.name ?? settings.sceneName ?? "", scene !== undefined && state.scenes?.program === id, scene === undefined, titled, /^#[0-9a-f]{6}$/i.test(settings.color ?? "") ? settings.color : undefined);
		}
		case "record": {
			const r = state.record;
			if (r?.state !== "recording") return recordKey("idle", undefined, titled);
			const ms = elapsed.ms + (elapsed.running ? now - elapsed.at : 0);
			return recordKey(r.paused ? "paused" : "recording", formatElapsed(ms), titled, pulse);
		}
		case "record-pause": {
			const r = state.record;
			return pauseKey(r?.state !== "recording" ? "idle" : r.paused ? "paused" : "recording", titled);
		}
		case "stream": {
			const s = state.stream;
			if (s?.state !== "live") return streamKey("idle", titled);
			const sub = ctx.liveSince > 0 ? formatElapsed(now - ctx.liveSince) : undefined;
			return streamKey(s.connected ? "live" : (s.connects ?? 0) > 0 ? "reconnecting" : "connecting", titled, sub, pulse);
		}
		case "source-visible": {
			const src = state.sources?.find((s) => String(s.id) === settings.source);
			const visible = src && src.item !== undefined ? src.visible !== false : undefined;
			return visibleKey(src?.name ?? settings.sourceName ?? "", visible, titled);
		}
		case "scene-step": {
			const dir = settings.direction === "previous" ? "previous" : "next";
			return sceneStepKey(dir, stepTarget(state, dir)?.name, titled);
		}
		case "replay": {
			const r = state.replay;
			return replayKey(r?.saving ? "saving" : r?.armed ? "armed" : "off", titled);
		}
		case "vcam":
			return vcamKey(state.vcam?.sending ? "on" : "off", titled);
		case "marker":
			return markerKey(state.record?.state === "recording", titled);
		case "volume":
			return ""; // a dial: drawn by `dialView`, not as a picture
		case "mute": {
			if (settings.source === "master") return muteKey("Master", state.scenes ? state.scenes.master?.muted === true : undefined, titled);
			const src = state.sources?.find((s) => String(s.id) === settings.source);
			return muteKey(src?.name ?? settings.sourceName ?? "", src ? src.muted === true : undefined, titled);
		}
	}
}
