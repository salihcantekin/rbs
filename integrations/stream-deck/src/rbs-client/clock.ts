/** Timers the client uses, so tests drive time themselves (ADR-0032). */
export interface Clock {
	/** Runs `fn` once after `ms`; returns a function that cancels it. */
	setTimeout(fn: () => void, ms: number): () => void;
}

/** The real clock. */
export const systemClock: Clock = {
	setTimeout(fn, ms) {
		const t = setTimeout(fn, ms);
		return () => clearTimeout(t);
	},
};

/** A clock that only moves when the test says so. */
export class ManualClock implements Clock {
	private now = 0;
	private seq = 0;
	private timers = new Map<number, { at: number; fn: () => void }>();

	setTimeout(fn: () => void, ms: number): () => void {
		const id = this.seq++;
		this.timers.set(id, { at: this.now + ms, fn });
		return () => void this.timers.delete(id);
	}

	/** Number of timers waiting. */
	get pending(): number {
		return this.timers.size;
	}

	/** The delays of the timers waiting, soonest first. */
	get delays(): number[] {
		return [...this.timers.values()].map((t) => t.at - this.now).sort((a, b) => a - b);
	}

	/** Moves time forward and runs the timers that fall due, in order. */
	advance(ms: number): void {
		const target = this.now + ms;
		for (;;) {
			let next: [number, { at: number; fn: () => void }] | undefined;
			for (const e of this.timers) if (e[1].at <= target && (!next || e[1].at < next[1].at)) next = e;
			if (!next) break;
			this.timers.delete(next[0]);
			this.now = next[1].at;
			next[1].fn();
		}
		this.now = target;
	}
}
