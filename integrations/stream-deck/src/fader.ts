// The volume fader's scale, the same as RBS's (crates/rbs-ui/src/fader.rs): the position is
// linear in dB from FLOOR_DB (left end) to 0 dB (right end); the left end itself is silence.
// A dial step moves the position, so a 10 percent step looks like 10 percent on RBS's fader.

export const FLOOR_DB = -60;

/** The fader position (0 to 1) of a linear volume factor. */
export function positionOf(volume: number): number {
	if (!Number.isFinite(volume) || volume <= 0) return 0;
	return Math.min(1, Math.max(0, (20 * Math.log10(volume) - FLOOR_DB) / -FLOOR_DB));
}

/** The linear volume factor at a fader position. */
export function volumeAt(pos: number): number {
	if (!Number.isFinite(pos) || pos <= 0) return 0;
	const db = FLOOR_DB + Math.min(1, pos) * -FLOOR_DB;
	return Math.min(1, 10 ** (db / 20));
}

/** The volume in words, as RBS shows it: "0 dB", "-6.0 dB", "Silent". */
export function volumeText(volume: number): string {
	if (!Number.isFinite(volume) || volume <= 0) return "Silent";
	const db = 20 * Math.log10(volume);
	return db > -0.05 ? "0 dB" : `${db.toFixed(1)} dB`;
}
