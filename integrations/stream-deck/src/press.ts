import { RbsError } from "./rbs-client/protocol.js";
import type { RbsClient } from "./rbs-client/client.js";
import type { Kind, Settings, ViewState } from "./view.js";

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
				const profile = settings.profile || client.state.profiles?.[0]?.id;
				if (!profile) throw new Error("RBS has no recording profile.");
				await client.call("record.start", { profile });
			}
			return;
		}
		case "stream": {
			if (state.stream?.state === "live") await client.call("stream.stop", {});
			else await client.call("stream.start", {});
			return;
		}
		case "source-visible": {
			const src = state.sources?.find((s) => String(s.id) === settings.source);
			if (!src || src.item === undefined) throw new Error("The source is not in the program scene.");
			await client.call("item.set_visible", { item: src.item, visible: src.visible === false } as never);
			return;
		}
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
