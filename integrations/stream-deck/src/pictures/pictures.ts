import { tokens as t } from "./tokens.js";

/** The state of a link as a key shows it. */
export type LinkText = { title: string; hint?: string };

export function esc(s: string): string {
	return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

/** Cuts text to at most two lines of `maxChars`, ellipsised. */
export function fit(text: string): string[] {
	const words = text.trim().split(/\s+/).filter(Boolean);
	const lines: string[] = [];
	let cur = "";
	for (const w of words) {
		if (!cur) cur = w;
		else if ((cur + " " + w).length <= t.maxChars) cur += " " + w;
		else {
			lines.push(cur);
			cur = w;
		}
	}
	if (cur) lines.push(cur);
	const out = lines.slice(0, 2).map((l) => (l.length > t.maxChars ? l.slice(0, t.maxChars - 1) + "…" : l));
	if (lines.length > 2) out[1] = (out[1] ?? "").replace(/.?$/, "…");
	return out;
}

interface Frame {
	bg: string;
	fg?: string;
	icon: string;
	label: string;
	/** Smaller second text under the icon (for example the elapsed time). */
	sub?: string;
	dim?: boolean;
	/** The user set a Stream Deck title: it is drawn by the app, so draw no text. */
	untitled?: boolean;
}

function frame(f: Frame): string {
	const fg = f.dim ? t.fgDim : (f.fg ?? t.fg);
	const lines = fit(f.label);
	const font = `font-family="Segoe UI, Helvetica, Arial, sans-serif" fill="${fg}"`;
	const row = (y: number, txt: string, weight: number) =>
		`<text x="72" y="${y}" text-anchor="middle" ${font} font-size="${t.labelSize}" font-weight="${weight}">${esc(txt)}</text>`;
	const text = f.untitled
		? ""
		: f.sub
		? row(103, lines[0] ?? "", 600) + row(128, f.sub, 400)
		: lines.map((l, i) => row((lines.length > 1 ? 106 : 120) + i * t.labelLineHeight, l, 600)).join("");
	return (
		`<svg xmlns="http://www.w3.org/2000/svg" width="${t.size}" height="${t.size}" viewBox="0 0 ${t.size} ${t.size}">` +
		`<rect width="${t.size}" height="${t.size}" rx="${t.radius}" fill="${f.bg}"/>` +
		`<g transform="translate(40 14)" fill="none" stroke="${fg}" stroke-width="5" stroke-linecap="round" stroke-linejoin="round">${f.icon}</g>` +
		text +
		`</svg>`
	);
}

const sceneIcon = `<rect x="2" y="8" width="60" height="42" rx="6"/><path d="M20 58h24"/>`;
const eyeOpen = `<path d="M2 30c10-16 20-22 30-22s20 6 30 22c-10 16-20 22-30 22S12 46 2 30z"/><circle cx="32" cy="30" r="9"/>`;
const eyeShut = `<path d="M2 30c10-16 20-22 30-22s20 6 30 22c-10 16-20 22-30 22S12 46 2 30z"/><path d="M8 54L56 6"/>`;
const speaker = `<path d="M6 22h12l16-14v44L18 38H6z"/>`;
const speakerOn = `${speaker}<path d="M44 20c6 6 6 18 0 24M52 12c10 10 10 30 0 40"/>`;
const speakerOff = `${speaker}<path d="M44 22l16 16M60 22L44 38"/>`;

export function sceneKey(name: string, active: boolean, missing: boolean, untitled = false): string {
	return frame({ bg: active ? t.bgActive : t.bg, icon: sceneIcon.replace("<rect", active ? `<rect fill="${t.accent}" fill-opacity=".35" stroke="${t.accent}"` : "<rect"), label: missing ? "Missing scene" : name, dim: missing, untitled });
}

export function recordKey(kind: "idle" | "recording" | "paused", elapsed?: string, untitled = false): string {
	const dot = `<circle cx="32" cy="30" r="22"/>`;
	if (kind === "idle") return frame({ bg: t.bg, icon: dot + `<circle cx="32" cy="30" r="9" fill="${t.fg}"/>`, label: "Record", untitled });
	const pause = `<path d="M22 18v24M42 18v24"/>`;
	return frame({ bg: t.bgLive, fg: t.fg, icon: kind === "paused" ? dot + pause : `<circle cx="32" cy="30" r="22" stroke="${t.live}"/><circle cx="32" cy="30" r="12" fill="${t.live}" stroke="${t.live}"/>`, label: "Stop", sub: elapsed, untitled });
}

export function streamKey(kind: "idle" | "connecting" | "live" | "reconnecting", untitled = false): string {
	const waves = `<circle cx="32" cy="30" r="6" fill="${t.fg}"/><path d="M18 16a20 20 0 0 0 0 28M46 16a20 20 0 0 1 0 28M8 8a34 34 0 0 0 0 44M56 8a34 34 0 0 1 0 44"/>`;
	if (kind === "idle") return frame({ bg: t.bg, icon: waves, label: "Stream", untitled });
	const red = waves.replace(/fill="[^"]*"/, `fill="${t.live}"`);
	return frame({ bg: t.bgLive, icon: `<g stroke="${t.live}">${red}</g>`, label: "Stop", sub: kind === "live" ? "Live" : kind === "connecting" ? "Connecting" : "Reconnecting", untitled });
}

export function visibleKey(name: string, visible: boolean | undefined, untitled = false): string {
	if (visible === undefined) return frame({ bg: t.bgDim, icon: eyeShut, label: name || "Source", sub: "Not in scene", dim: true, untitled });
	return frame({ bg: t.bg, icon: visible ? eyeOpen : eyeShut, label: name || "Source", dim: !visible, untitled });
}

export function muteKey(name: string, muted: boolean | undefined, untitled = false): string {
	if (muted === undefined) return frame({ bg: t.bg, icon: speaker, label: name || "Mute", dim: true, untitled });
	return frame({ bg: muted ? t.bgDim : t.bg, icon: muted ? speakerOff : speakerOn, label: name || "Mute", dim: muted, untitled });
}

/** A key whose link to RBS is down. */
export function linkKey(text: LinkText): string {
	const warn = `<circle cx="32" cy="30" r="24"/><path d="M32 16v18M32 42v2"/>`;
	return frame({ bg: t.bgDim, fg: t.warn, icon: warn, label: text.title });
}

export function toDataUrl(svg: string): string {
	return `data:image/svg+xml;charset=utf8,${encodeURIComponent(svg)}`;
}

/** Pause or resume a recording. */
export function pauseKey(kind: "idle" | "recording" | "paused", untitled = false): string {
	const bars = `<path d="M22 16v28M42 16v28"/>`;
	const play = `<path d="M24 14l26 16-26 16z"/>`;
	if (kind === "idle") return frame({ bg: t.bgDim, icon: bars, label: "Pause", dim: true, untitled });
	if (kind === "paused") return frame({ bg: t.bgLive, icon: play, label: "Resume", untitled });
	return frame({ bg: t.bg, icon: bars, label: "Pause", untitled });
}
