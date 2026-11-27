import streamDeck, { type DidReceiveSettingsEvent, type TitleParametersDidChangeEvent, type KeyDownEvent, type SendToPluginEvent, SingletonAction, type WillAppearEvent, type WillDisappearEvent } from "@elgato/streamdeck";

import type { JsonObject } from "@elgato/utils";
import { toDataUrl } from "../pictures/pictures.js";
import { explain, press } from "../press.js";
import { client, elapsed, lease } from "../runtime.js";
import { type Kind, type Settings, type ViewState, linkText, renderKey } from "../view.js";

/** Every action instance, so a state change repaints all keys. */
const instances = new Set<RbsKeyAction>();
/** What each visible key last got, so unchanged pictures are not sent again. */
const shown = new Map<string, string>();
const known = new Map<string, { settings: Settings; action: WillAppearEvent["action"] }>();
const lastError = new Map<string, string>();
/** Keys whose user set a Stream Deck title: the app draws it, so the picture has no text. */
const titled = new Set<string>();
const busy = new Set<string>();
let tick: ReturnType<typeof setInterval> | undefined;

function repaintAll(): void {
	const now = Date.now();
	for (const [id, { settings, action }] of known) {
		const kind = [...instances].find((i) => i.actions.some((a) => a.id === id))?.kind;
		if (!kind || !action.isKey()) continue;
		const svg = toDataUrl(renderKey(kind, settings, client.status, client.state as unknown as ViewState, elapsed, now, titled.has(id)));
		if (shown.get(id) === svg) continue;
		shown.set(id, svg);
		void action.setImage(svg);
	}
	updateTick();
	void sendLists();
}

/** The 1 s timer for the elapsed time exists only while a recording runs and a key shows it (P13). */
function updateTick(): void {
	const need = elapsed.running && [...instances].some((i) => i.kind === "record" && [...i.actions].length > 0);
	if (need && !tick) tick = setInterval(repaintAll, 1000);
	else if (!need && tick) {
		clearInterval(tick);
		tick = undefined;
	}
}

function items(list: { id: number | string; name: string }[]): { label: string; value: string }[] {
	return list.map((x) => ({ label: x.name, value: String(x.id) }));
}

/** Answers the inspector's data sources and status line. */
async function sendLists(): Promise<void> {
	if (!streamDeck.ui.action) return;
	const s = client.state as unknown as ViewState & { profiles?: { id: string; name: string }[] };
	const link = client.status;
	const id = streamDeck.ui.action.id;
	const err = lastError.get(id);
	const status =
		link.state === "ready"
			? `Connected to ${link.server ?? "RBS"} (protocol ${link.protocol})${link.scopes?.includes("control") ? "" : ". This token is view-only: keys will not work."}`
			: link.state === "unreachable"
				? "Not connected. Is RBS running with Settings, Remote control, Allow remote control on?"
				: linkText(link).title + ".";
	await streamDeck.ui.sendToPropertyInspector({ event: "status", status, error: err ?? "" });
	await streamDeck.ui.sendToPropertyInspector({ event: "getScenes", items: items(s.scenes?.scenes ?? []) });
	// Every source of the collection, not only the scene on air: a key for a source that is
	// not in the scene on air does nothing until it is.
	const all = items(s.scenes?.sources ?? s.sources ?? []);
	await streamDeck.ui.sendToPropertyInspector({ event: "getSources", items: all });
	await streamDeck.ui.sendToPropertyInspector({ event: "getMuteTargets", items: [{ label: "Master", value: "master" }, ...all] });
}

client.onStatus(repaintAll);
client.onState(repaintAll);

/** Base of every RBS key action (docs/specs/stream-deck.md, Actions). */
export abstract class RbsKeyAction extends SingletonAction<JsonObject> {
	abstract readonly kind: Kind;

	constructor() {
		super();
		instances.add(this as unknown as RbsKeyAction);
	}

	override onWillAppear(ev: WillAppearEvent<JsonObject>): void {
		known.set(ev.action.id, { settings: ev.payload.settings as Settings, action: ev.action });
		lease.acquire(ev.action.id);
		repaintAll();
	}

	override onWillDisappear(ev: WillDisappearEvent<JsonObject>): void {
		known.delete(ev.action.id);
		shown.delete(ev.action.id);
		lastError.delete(ev.action.id);
		titled.delete(ev.action.id);
		lease.release(ev.action.id);
		updateTick();
	}

	override onDidReceiveSettings(ev: DidReceiveSettingsEvent<JsonObject>): void {
		const k = known.get(ev.action.id);
		if (k) k.settings = ev.payload.settings as Settings;
		shown.delete(ev.action.id);
		repaintAll();
	}

	override onTitleParametersDidChange(ev: TitleParametersDidChangeEvent<JsonObject>): void {
		if (ev.payload.title) titled.add(ev.action.id);
		else titled.delete(ev.action.id);
		shown.delete(ev.action.id);
		repaintAll();
	}

	override onPropertyInspectorDidAppear(): void {
		void sendLists();
	}

	override onSendToPlugin(_ev: SendToPluginEvent<never, JsonObject>): void {
		// Data sources are answered by `sendLists`, which runs on every state change.
		void sendLists();
	}

	override async onKeyDown(ev: KeyDownEvent<JsonObject>): Promise<void> {
		const id = ev.action.id;
		if (busy.has(id)) return;
		if (client.status.state !== "ready") {
			client.retryNow();
			await ev.action.showAlert();
			return;
		}
		busy.add(id);
		try {
			await press(this.kind, ev.payload.settings as Settings, client);
			lastError.delete(id);
		} catch (err) {
			lastError.set(id, explain(err));
			streamDeck.logger.warn(`${this.kind} press failed: ${explain(err)}`);
			await ev.action.showAlert();
		} finally {
			busy.delete(id);
			void sendLists();
		}
	}
}
