import type { LinkStatus } from "./rbs-client/client.js";
import { type LinkText, linkKey, muteKey, pauseKey, recordKey, sceneKey, streamKey, visibleKey } from "./pictures/pictures.js";

/** Settings of each action (stored by Stream Deck; ids are strings from the inspector). */
export interface SceneSettings { scene?: string; sceneName?: string }
export interface SourceSettings { source?: string; sourceName?: string }
export type Settings = SceneSettings & SourceSettings;
export type Kind = "scene" | "record" | "record-pause" | "stream" | "source-visible" | "mute";

/** The parts of RBS's state the keys read, loosely typed (the server may add fields). */
export interface ViewState {
	scenes?: { scenes: { id: number; name: string }[]; program: number; master?: { muted?: boolean }; sources?: { id: number; name: string }[] };
	sources?: { id: number; name: string; item?: number; visible?: boolean; muted?: boolean }[];
	record?: { state: string; paused?: boolean };
	stream?: { state: string; connected?: boolean; connects?: number };
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

export function formatElapsed(ms: number): string {
	const total = Math.max(0, Math.floor(ms / 1000));
	const h = Math.floor(total / 3600);
	const m = Math.floor((total % 3600) / 60);
	const s = total % 60;
	const two = (n: number) => String(n).padStart(2, "0");
	return h > 0 ? `${h}:${two(m)}:${two(s)}` : `${two(m)}:${two(s)}`;
}

/** The SVG of one key. Pure: everything it needs is passed in. */
export function renderKey(kind: Kind, settings: Settings, link: LinkStatus, state: ViewState, elapsed: Elapsed, now: number, titled = false): string {
	if (link.state !== "ready") return linkKey(linkText(link));
	switch (kind) {
		case "scene": {
			const id = Number(settings.scene);
			const scene = state.scenes?.scenes.find((s) => s.id === id);
			return sceneKey(scene?.name ?? settings.sceneName ?? "", scene !== undefined && state.scenes?.program === id, scene === undefined, titled);
		}
		case "record": {
			const r = state.record;
			if (r?.state !== "recording") return recordKey("idle", undefined, titled);
			const ms = elapsed.ms + (elapsed.running ? now - elapsed.at : 0);
			return recordKey(r.paused ? "paused" : "recording", formatElapsed(ms), titled);
		}
		case "record-pause": {
			const r = state.record;
			return pauseKey(r?.state !== "recording" ? "idle" : r.paused ? "paused" : "recording", titled);
		}
		case "stream": {
			const s = state.stream;
			if (s?.state !== "live") return streamKey("idle", titled);
			return streamKey(s.connected ? "live" : (s.connects ?? 0) > 0 ? "reconnecting" : "connecting", titled);
		}
		case "source-visible": {
			const src = state.sources?.find((s) => String(s.id) === settings.source);
			const visible = src && src.item !== undefined ? src.visible !== false : undefined;
			return visibleKey(src?.name ?? settings.sourceName ?? "", visible, titled);
		}
		case "mute": {
			if (settings.source === "master") return muteKey("Master", state.scenes ? state.scenes.master?.muted === true : undefined, titled);
			const src = state.sources?.find((s) => String(s.id) === settings.source);
			return muteKey(src?.name ?? settings.sourceName ?? "", src ? src.muted === true : undefined, titled);
		}
	}
}
