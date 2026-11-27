import { RbsError } from "./rbs-client/protocol.js";
import type { RbsClient } from "./rbs-client/client.js";
import { positionOf, volumeAt } from "./fader.js";
import { type Kind, type Settings, type ViewState, findFilter, modeOf, stepTarget } from "./view.js";

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

/** Starts a recording with the profile RBS has selected: the user chose it in RBS, not on the key. */
async function startRecording(client: RbsClient): Promise<void> {
	const current = (await client.call("profile.list", {})).current;
	const known = client.state.profiles ?? [];
	const profile = known.some((p) => p.id === current) ? current : known[0]?.id;
	if (!profile) throw new Error("RBS has no recording profile.");
	await client.call("record.start", { profile });
}

/** After a switch the lists are read again, once now and once the app has loaded the entry. */
function refreshSoon(client: RbsClient): void {
	void client.refreshLibraries();
	for (const ms of [1500, 5000]) setTimeout(() => void client.refreshLibraries(), ms).unref?.();
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
			const recording = state.record?.state === "recording";
			const mode = modeOf(settings);
			if ((mode === "on" && recording) || (mode === "off" && !recording)) return; // not this key's job
			if (recording) await client.call("record.stop", {});
			else await startRecording(client);
			return;
		}
		case "studio": {
			const on = state.scenes?.preview !== undefined;
			const mode = modeOf(settings);
			if ((mode === "on" && on) || (mode === "off" && !on)) return;
			// Entering studio mode previews the scene on air; leaving clears the preview.
			if (on) await client.call("canvas.set_preview", { scene: null });
			else if (state.scenes) await client.call("canvas.set_preview", { scene: state.scenes.program });
			return;
		}
		case "transition": {
			if (state.scenes?.preview === undefined) return; // outside studio mode there is nothing to put on air
			await client.call("canvas.transition", {});
			return;
		}
		case "scene-step": {
			const target = stepTarget(state, settings.direction === "previous" ? "previous" : "next");
			if (!target) return;
			// In studio mode the app's Next scene changes the preview, not the program.
			if (state.scenes?.preview !== undefined) await client.call("canvas.set_preview", { scene: target.id });
			else await client.call("scene.switch", { scene: target.id });
			return;
		}
		case "replay": {
			if (!state.replay?.armed) throw new Error("The replay buffer is off in RBS.");
			await client.call("replay.save", {});
			return;
		}
		case "vcam": {
			const on = state.vcam?.sending === true;
			const mode = modeOf(settings);
			if ((mode === "on" && on) || (mode === "off" && !on)) return;
			if (on) await client.call("vcam.stop", {});
			else await client.call("vcam.start", {});
			return;
		}
		case "replay-buffer": {
			if (!state.replay) throw new Error("This RBS has no replay buffer.");
			const on = state.replay.armed === true;
			const mode = modeOf(settings);
			if ((mode === "on" && on) || (mode === "off" && !on)) return;
			if (on) await client.call("replay.disarm", {});
			else await client.call("replay.arm", {});
			return;
		}
		case "collection":
		case "profile": {
			if (!settings.entry) throw new Error("Choose one in the inspector.");
			const lib = kind === "collection" ? state.collections : state.profileLibrary;
			if (lib?.current === settings.entry) return;
			// RBS refuses while recording or streaming ("conflict"), as the File menu does.
			await client.call(kind === "collection" ? "collection.switch" : "profile.switch", { id: settings.entry });
			refreshSoon(client);
			return;
		}
		case "preview": {
			if (state.scenes?.preview === undefined) return; // outside studio mode there is no preview
			const scene = Number(settings.scene);
			if (!Number.isInteger(scene)) throw new Error("Choose a scene in the inspector.");
			await client.call("canvas.set_preview", { scene });
			return;
		}
		case "status":
			return; // shows, does nothing
		case "go-live": {
			const recording = state.record?.state === "recording";
			const live = state.stream?.state === "live";
			const mode = modeOf(settings);
			if ((mode === "on" && (recording || live)) || (mode === "off" && !recording && !live)) return;
			if (recording || live) {
				if (recording) await client.call("record.stop", {});
				if (live) await client.call("stream.stop", {});
			} else {
				await startRecording(client);
				await client.call("stream.start", {});
			}
			return;
		}
		case "filter": {
			const id = Number(settings.filter);
			if (!Number.isInteger(id)) throw new Error("Choose a filter in the inspector.");
			const f = findFilter(state, settings.filter);
			if (!f) return; // not in the scene on air: its state is not known
			const mode = modeOf(settings);
			const want = mode === "toggle" ? !f.enabled : mode === "on";
			if (want !== f.enabled) await client.call("filter.set_enabled", { filter: id, enabled: want });
			return;
		}
		case "bus-volume": // a dial press mutes, like the mute key
		case "bus-mute": {
			const b = state.buses?.buses.find((x) => x.name === settings.bus);
			if (!settings.bus) throw new Error("Choose a track in the inspector.");
			if (!b) return;
			const mode = modeOf(settings);
			const want = kind === "bus-volume" || mode === "toggle" ? !b.muted : mode === "on";
			if (want !== b.muted) await client.call("bus.set", { name: settings.bus, muted: want } as never);
			return;
		}
		case "marker": {
			await client.call("marker.add", {});
			return;
		}
		case "record-pause": {
			if (state.record?.state !== "recording") return;
			const paused = state.record.paused === true;
			const mode = modeOf(settings);
			if (mode === "on") {
				if (!paused) await client.call("record.pause", {});
			} else if (mode === "off") {
				if (paused) await client.call("record.resume", {});
			} else await client.call("record.pause_toggle", {});
			return;
		}
		case "stream": {
			const live = state.stream?.state === "live";
			const mode = modeOf(settings);
			if ((mode === "on" && live) || (mode === "off" && !live)) return;
			if (live) await client.call("stream.stop", {});
			else await client.call("stream.start", {});
			return;
		}
		case "source-visible": {
			const src = state.sources?.find((s) => String(s.id) === settings.source);
			if (!src || src.item === undefined) return; // not in the scene on air: nothing to do
			const visible = src.visible !== false;
			const want = modeOf(settings) === "toggle" ? !visible : modeOf(settings) === "on";
			if (want !== visible) await client.call("item.set_visible", { item: src.item, visible: want } as never);
			return;
		}
		case "volume": // a dial press mutes, like the mute key
		case "mute": {
			if (settings.source === "master") {
				const muted = state.scenes?.master?.muted === true;
				const want = modeOf(settings) === "toggle" ? !muted : modeOf(settings) === "on";
				if (want !== muted) await client.call("canvas.set_master_muted", { muted: want });
				return;
			}
			const src = state.sources?.find((s) => String(s.id) === settings.source);
			const source = Number(settings.source);
			if (!Number.isInteger(source)) throw new Error("Choose a source in the inspector.");
			const muted = src?.muted === true;
			const mode = modeOf(settings);
			const want = kind === "volume" || mode === "toggle" ? (src ? !muted : true) : mode === "on";
			if (!src || want !== muted) await client.call("source.set_muted", { source, muted: want });
			return;
		}
	}
}

/** A dial turn: the new volume (a linear 0 to 1 factor) for `ticks` steps of `step` percent. */
export async function turnVolume(kind: Kind, settings: Settings, ticks: number, client: RbsClient): Promise<void> {
	const state = client.state as unknown as ViewState;
	const step = Math.min(25, Math.max(1, Number(settings.step) || 5)) / 100;
	if (kind === "bus-volume") {
		const b = state.buses?.buses.find((x) => x.name === settings.bus);
		if (b) await client.call("bus.set", { name: b.name, volume: moved(b.volume, ticks, step) } as never);
		return;
	}
	if (settings.source === "master") {
		await client.call("canvas.set_master_volume", { volume: moved(state.scenes?.master?.volume ?? 1, ticks, step) });
		return;
	}
	const src = state.sources?.find((s) => String(s.id) === settings.source);
	if (!src) return; // not in the scene on air: its volume is not known
	await client.call("source.set_volume", { source: src.id, volume: moved(src.volume ?? 1, ticks, step) });
}

/** The volume after `ticks` steps on RBS's fader scale (steps are a share of the fader, not of the amplitude). */
function moved(volume: number, ticks: number, step: number): number {
	const pos = Math.min(1, Math.max(0, positionOf(volume) + ticks * step));
	return Math.round(volumeAt(pos) * 10000) / 10000;
}
