import streamDeck from "@elgato/streamdeck";

import { RbsClient } from "./rbs-client/client.js";
import { systemClock } from "./rbs-client/clock.js";
import { Lease } from "./lease.js";
import type { Elapsed, ViewState } from "./view.js";

export const PLUGIN_UUID = "com.rbs.streamdeck";
const VERSION = "0.1.0";

/** Plugin-wide settings, set in any action's inspector (ADR-0198 decision 3). */
export type GlobalSettings = {
	port?: string;
	token?: string;
};

export const client = new RbsClient({ clock: systemClock, version: VERSION });
export const lease = new Lease(systemClock, () => client.start(), () => client.stop());

/** Recording time anchor, updated on every `record.state` event. */
export const elapsed: Elapsed = { ms: 0, at: 0, running: false };
/** Live time anchor: when the stream was last seen to start. */
export const live = { since: 0, on: false };

client.onState((state, changed) => {
	if (changed === "stream" || changed === "profiles") {
		const on = (state as unknown as ViewState).stream?.state === "live";
		// The start time is known only for a start seen while connected.
		if (on && !live.on) live.since = changed === "stream" ? Date.now() : 0;
		live.on = on;
	}
	if (changed !== "record" && changed !== "profiles") return;
	const r = (state as unknown as ViewState).record as { state: string; recorded_ms?: number; paused?: boolean } | undefined;
	elapsed.ms = r?.recorded_ms ?? 0;
	elapsed.at = Date.now();
	elapsed.running = r?.state === "recording" && r.paused !== true;
});

export function applyGlobalSettings(g: GlobalSettings): void {
	const port = Number(g.port);
	client.configure(Number.isInteger(port) && port > 0 && port < 65536 ? port : 4456, (g.token ?? "").trim());
}

export async function loadGlobalSettings(): Promise<void> {
	applyGlobalSettings(await streamDeck.settings.getGlobalSettings<GlobalSettings>());
	streamDeck.settings.onDidReceiveGlobalSettings<GlobalSettings>((ev) => applyGlobalSettings(ev.settings));
}
