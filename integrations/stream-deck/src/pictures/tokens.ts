/**
 * Colours and sizes of the key pictures, in one place (ADR-0198 decision 6). RBS uses red
 * only for something that records or is live; the accent marks the active scene.
 */
export const tokens = {
	size: 144,
	radius: 22,
	bg: "#1d1f24",
	bgActive: "#16355f",
	bgLive: "#4a1519",
	bgDim: "#17181c",
	fg: "#f2f3f5",
	fgDim: "#7c818b",
	accent: "#4c9aff",
	live: "#ff4d57",
	warn: "#f5b942",
	iconBox: 64,
	labelSize: 19,
	labelLineHeight: 23,
	maxChars: 12,
} as const;
