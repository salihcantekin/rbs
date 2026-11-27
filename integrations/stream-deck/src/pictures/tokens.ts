/**
 * Colours and sizes of the key pictures, in one place (ADR-0198 decision 6). RBS uses red
 * only for something that records or is live; the accent marks what is on air.
 */
export const tokens = {
	size: 144,
	radius: 24,
	fg: "#f4f5f7",
	fgDim: "#7b808b",
	accent: "#4c9aff",
	live: "#ff4d57",
	warn: "#f5b942",
	/** Top and bottom colour of the background per tone. */
	bg: {
		neutral: ["#2d3038", "#1a1c21"],
		dim: ["#22242a", "#16171b"],
		live: ["#6b1d25", "#320f14"],
		warn: ["#2d2a22", "#1a1914"],
	},
	badgeRadius: 31,
	badgeY: 52,
	labelSize: 19,
	subSize: 17,
	labelLineHeight: 23,
	maxChars: 12,
} as const;
