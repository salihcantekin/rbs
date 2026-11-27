import type { Clock } from "./rbs-client/clock.js";

/**
 * Counts the visible RBS keys (ADR-0198 decision 5). `onFirst` runs when the first key
 * appears; `onIdle` runs `graceMs` after the last one disappeared, unless a key appeared
 * again meanwhile (page switches on the deck must not reconnect).
 */
export class Lease {
	private readonly holders = new Set<string>();
	private cancelIdle?: () => void;
	private active = false;

	constructor(
		private readonly clock: Clock,
		private readonly onFirst: () => void,
		private readonly onIdle: () => void,
		private readonly graceMs = 10_000,
	) {}

	get count(): number {
		return this.holders.size;
	}

	acquire(id: string): void {
		this.cancelIdle?.();
		this.cancelIdle = undefined;
		this.holders.add(id);
		if (!this.active) {
			this.active = true;
			this.onFirst();
		}
	}

	release(id: string): void {
		if (!this.holders.delete(id) || this.holders.size > 0) return;
		this.cancelIdle = this.clock.setTimeout(() => {
			this.cancelIdle = undefined;
			if (this.holders.size === 0) {
				this.active = false;
				this.onIdle();
			}
		}, this.graceMs);
	}
}
