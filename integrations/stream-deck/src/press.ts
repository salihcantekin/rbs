import { RbsError } from "./rbs-client/protocol.js";
import type { RbsClient } from "./rbs-client/client.js";
import { type Kind, type Settings, type ViewState, stepTarget } from "./view.js";

/** Why a press failed, in words for the inspector's status line. */
export function explain(err: unknown): string {
	if (err instanceof RbsError) {
		if (err.kind === "scope_denied") return "This token cannot do this: it needs Control access.";
		if (err.kind === "not_permitted") return "Turn on \"Allow actions that cannot be undone\" for this token in RBS (Settings, Remote control).";
		if (err.kind === "not_found") return "RBS no longer has this scene or source.";
		if (err.kind === "conflict") return "RBS refused: that is already the state, or the output is busy.";
		return `RBS refused: ${err.kind}${err.reason ? ` (${err.reason})` : ""}.`;
	}
	return err instanceof Error ? err.message : String(err);
}

/** What a press does (docs/specs/stream-deck.md, Actions). Throws when the call fails. */
export async function press(kind: Kind, settings: Settings, client: RbsClient): Promise<void> {
	const state = client.state as unknown as ViewState;
	switch (kind) {
		case "scene": {
			const scene = Number(settings.scene);
			if (!Number.isInteger(scene)) throw new Error("Choose a scene in the inspector.");
			await client.call("scene.switch", { scene });
			return;
		}
		case "record": {
			if (state.record?.state === "recording") await client.call("record.stop", {});
			else {
				// The profile RBS has selected: the user chose it in RBS, not on the key.
				const current = (await client.call("profile.list", {})).current;
				const known = client.state.profiles ?? [];
				const profile = known.some((p) => p.id === current) ? current : known[0]?.id;
				if (!profile) throw new Error("RBS has no recording profile.");
				await client.call("record.start", { profile });
			}
			return;
		}
		case "scene-step": {
			const target = stepTarget(state, settings.direction === "previous" ? "previous" : "next");
			if (target) await client.call("scene.switch", { scene: target.id });
			return;
		}
		case "replay": {
			if (!state.replay?.armed) throw new Error("The replay buffer is off in RBS.");
			await client.call("replay.save", {});
			return;
		}
		case "vcam": {
			if (state.vcam?.sending) await client.call("vcam.stop", {});
			else await client.call("vcam.start", {});
			return;
		}
		case "marker": {
			await client.call("marker.add", {});
			return;
		}
		case "record-pause": {
			if (state.record?.state !== "recording") return;
			await client.call("record.pause_toggle", {});
			return;
		}
		case "stream": {
			if (state.stream?.state === "live") await client.call("stream.stop", {});
			else await client.call("stream.start", {});
			return;
		}
		case "source-visible": {
			const src = state.sources?.find((s) => String(s.id) === settings.source);
			if (!src || src.item === undefined) return; // not in the scene on air: nothing to do
			await client.call("item.set_visible", { item: src.item, visible: src.visible === false } as never);
			return;
		}
		case "volume": // a dial press mutes, like the mute key
		case "mute": {
			if (settings.source === "master") {
				await client.call("canvas.set_master_muted", { muted: state.scenes?.master?.muted !== true });
				return;
			}
			const src = state.sources?.find((s) => String(s.id) === settings.source);
			const source = Number(settings.source);
			if (!Number.isInteger(source)) throw new Error("Choose a source in the inspector.");
			await client.call("source.set_muted", { source, muted: src ? src.muted !== true : true });
			return;
		}
	}
}

/** A dial turn: the new volume (a linear 0 to 1 factor) for `ticks` steps of `step` percent. */
export async function turnVolume(settings: Settings, ticks: number, client: RbsClient): Promise<void> {
	const state = client.state as unknown as ViewState;
	const step = Math.min(25, Math.max(1, Number(settings.step) || 5)) / 100;
	if (settings.source === "master") {
		const now = state.scenes?.master?.volume ?? 1;
		await client.call("canvas.set_master_volume", { volume: clamp(now + ticks * step) });
		return;
	}
	const src = state.sources?.find((s) => String(s.id) === settings.source);
	if (!src) return; // not in the scene on air: its volume is not known
	await client.call("source.set_volume", { source: src.id, volume: clamp(Math.round((src.volume ?? 1) * 100 + ticks * step * 100) / 100) });
}

function clamp(v: number): number {
	return Math.min(1, Math.max(0, Math.round(v * 100) / 100));
}
