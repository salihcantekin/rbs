import { positionOf, volumeText } from "../fader.js";
import { tokens as t } from "./tokens.js";

/** The state of a link as a key shows it. */
export type LinkText = { title: string; hint?: string };

/** What a key looks like, before it is drawn. Tests compare these. */
export interface KeyLook {
	/** SVG shapes in a 64 by 64 box, drawn with the tone's colour. */
	glyph: string;
	label: string;
	/** A second, lighter line (time, target, status). */
	sub?: string;
	tone: "neutral" | "accent" | "live" | "dim" | "warn" | "go";
	/** Icon size class; default medium. */
	size?: "small" | "medium" | "large";
	/** Colour of an `accent` tone; defaults to the token. */
	accent?: string;
	/** Blinking ring (recording, live): `pulse` is the current phase. */
	ring?: boolean;
	pulse?: boolean;
	/** A small dot in the badge corner (what is on air). */
	dot?: boolean;
	/** The user set a Stream Deck title: it is drawn by the app, so draw no text. */
	untitled?: boolean;
}

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

const FONT = `font-family="Segoe UI, Helvetica, Arial, sans-serif"`;

/** Draws a key: a soft gradient tile, a round badge with the glyph, and one or two text lines. */
export function drawKey(k: KeyLook): string {
	const tone = k.tone;
	const main = tone === "live" ? t.live : tone === "accent" ? (k.accent ?? t.accent) : tone === "warn" ? t.warn : tone === "go" ? t.go : tone === "dim" ? t.fgDim : t.fg;
	const [top, bottom] = tone === "accent" ? [mix(k.accent ?? t.accent, "#1a1c21", 0.55), mix(k.accent ?? t.accent, "#101114", 0.18)] : t.bg[tone];
	const gid = `g${top.slice(1)}${bottom.slice(1)}`; // unique per colour pair, so tiles shown together do not share one gradient
	const text = tone === "dim" ? t.fgDim : tone === "warn" ? t.warn : t.fg;
	const sz = t.iconSize[k.size ?? "medium"];
	const lines = fit(k.label);
	const row = (y: number, txt: string, size: number, weight: number, fill: string, opacity = 1) =>
		`<text x="72" y="${y}" text-anchor="middle" ${FONT} font-size="${size}" font-weight="${weight}" fill="${fill}" fill-opacity="${opacity}">${esc(txt)}</text>`;
	const body = k.untitled
		? ""
		: k.sub
			? row(108, lines[0] ?? "", t.labelSize, 600, text) + row(129, k.sub, t.subSize, 400, text, 0.8)
			: lines.map((l, i) => row((lines.length > 1 ? 108 : 122) + i * t.labelLineHeight, l, t.labelSize, 600, text)).join("");
	// With no text the badge sits lower, centred in the tile.
	const cy = k.untitled ? 72 : sz.y;
	const ring = k.ring
		? `<circle cx="72" cy="${cy}" r="${sz.radius + 5}" fill="none" stroke="${main}" stroke-width="3" stroke-opacity="${k.pulse === false ? 0.15 : 0.7}"/>`
		: "";
	const dot = k.dot ? `<circle cx="${72 + 24}" cy="${cy - 24}" r="6" fill="${main}" stroke="${top}" stroke-width="2.5"/>` : "";
	return (
		`<svg xmlns="http://www.w3.org/2000/svg" width="${t.size}" height="${t.size}" viewBox="0 0 ${t.size} ${t.size}">` +
		`<defs><linearGradient id="${gid}" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="${top}"/><stop offset="1" stop-color="${bottom}"/></linearGradient></defs>` +
		`<rect width="${t.size}" height="${t.size}" rx="${t.radius}" fill="url(#${gid})"/>` +
		`<rect x="1.5" y="1.5" width="${t.size - 3}" height="${t.size - 3}" rx="${t.radius - 1.5}" fill="none" stroke="#fff" stroke-opacity=".09" stroke-width="3"/>` +
		ring +
		`<circle cx="72" cy="${cy}" r="${sz.radius}" fill="${main}" fill-opacity="${tone === "neutral" ? 0.1 : 0.18}"/>` +
		`<g transform="translate(${72 - 32 * sz.scale} ${cy - 32 * sz.scale}) scale(${sz.scale})" fill="none" stroke="${main}" stroke-width="${(6.5 / sz.scale) * 0.5625}" stroke-linecap="round" stroke-linejoin="round" color="${main}">${k.glyph}</g>` +
		dot +
		body +
		`</svg>`
	);
}

/** Mixes two #rrggbb colours: `amount` of `a`. */
function mix(a: string, b: string, amount: number): string {
	const p = (c: string, i: number) => parseInt(c.slice(1 + i * 2, 3 + i * 2), 16);
	const h = (i: number) => Math.round(p(a, i) * amount + p(b, i) * (1 - amount)).toString(16).padStart(2, "0");
	return `#${h(0)}${h(1)}${h(2)}`;
}

// ---- glyphs (64 by 64) -----------------------------------------------------------

export const glyphs = {
	scene: `<rect x="6" y="10" width="52" height="36" rx="7"/><path d="M22 56h20M32 46v10"/>`,
	recordIdle: `<circle cx="32" cy="32" r="18" fill="currentColor"/>`,
	stop: `<rect x="14" y="14" width="36" height="36" rx="8" fill="currentColor"/>`,
	pause: `<path d="M22 14v36M42 14v36"/>`,
	play: `<path d="M22 12l28 20-28 20z" fill="currentColor"/>`,
	stream: `<circle cx="32" cy="32" r="5" fill="currentColor"/><path d="M20 20a17 17 0 0 0 0 24M44 20a17 17 0 0 1 0 24M11 11a30 30 0 0 0 0 42M53 11a30 30 0 0 1 0 42"/>`,
	eyeOpen: `<path d="M3 32C12 17 22 10 32 10s20 7 29 22C52 47 42 54 32 54S12 47 3 32z"/><circle cx="32" cy="32" r="9" fill="currentColor"/>`,
	eyeShut: `<path d="M3 32C12 17 22 10 32 10s20 7 29 22C52 47 42 54 32 54S12 47 3 32z"/><path d="M10 56L54 8"/>`,
	speaker: `<path d="M8 24h12l16-14v44L20 40H8z" fill="currentColor"/>`,
	speakerOn: `<path d="M8 24h12l16-14v44L20 40H8z" fill="currentColor"/><path d="M45 22c6 6 6 14 0 20M53 14c11 11 11 25 0 36"/>`,
	speakerOff: `<path d="M8 24h12l16-14v44L20 40H8z" fill="currentColor"/><path d="M46 24l14 16M60 24L46 40"/>`,
	next: `<path d="M8 32h46M36 14l18 18-18 18"/>`,
	previous: `<path d="M56 32H10M28 14L10 32l18 18"/>`,
	replay: `<path d="M32 10a22 22 0 1 1-20 13M8 8v18h18"/><path d="M27 24l14 8-14 8z" fill="currentColor"/>`,
	camera: `<circle cx="32" cy="26" r="18"/><circle cx="32" cy="26" r="7" fill="currentColor"/><path d="M20 58h24M32 44v14"/>`,
	marker: `<path d="M14 8v50M14 10h34l-9 12 9 12H14" /><path d="M14 10h34l-9 12 9 12H14z" fill="currentColor" fill-opacity=".35"/>`,
	studio: `<rect x="3" y="14" width="26" height="36" rx="6"/><rect x="35" y="14" width="26" height="36" rx="6" fill="currentColor" fill-opacity=".35"/>`,
	transition: `<path d="M8 22h44M40 10l12 12-12 12M56 42H12M24 30L12 42l12 12"/>`,
	warn: `<circle cx="32" cy="32" r="24"/><path d="M32 18v18M32 44v2"/>`,
	volume: `<path d="M8 24h12l16-14v44L20 40H8z" fill="currentColor"/><path d="M45 22c6 6 6 14 0 20"/>`,
};

/** A key whose link to RBS is down. */
export function linkKey(text: LinkText): string {
	return drawKey({ glyph: glyphs.warn, label: text.title, tone: "warn" });
}

export function toDataUrl(svg: string): string {
	return `data:image/svg+xml;charset=utf8,${encodeURIComponent(svg)}`;
}

/** Text of a dial's touch strip (the built-in $B1 layout). */
export function dialFeedback(title: string, volume: number | undefined, muted: boolean | undefined): { title: string; value: string; indicator: { value: number } } {
	if (volume === undefined) return { title, value: "-", indicator: { value: 0 } };
	return { title, value: muted ? "Muted" : volumeText(volume), indicator: { value: muted ? 0 : Math.round(positionOf(volume) * 100) } };
}
