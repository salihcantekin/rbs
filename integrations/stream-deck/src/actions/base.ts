import streamDeck, { type DialDownEvent, type DialRotateEvent, type DidReceiveSettingsEvent, type TitleParametersDidChangeEvent, type KeyDownEvent, type SendToPluginEvent, SingletonAction, type WillAppearEvent, type WillDisappearEvent } from "@elgato/streamdeck";

import type { JsonObject } from "@elgato/utils";
import { dialFeedback, toDataUrl } from "../pictures/pictures.js";
import { explain, press, turnVolume } from "../press.js";
import { client, elapsed, lease, live } from "../runtime.js";
import { type Kind, NOTICE_MS, type Settings, type ViewState, dialView, filterName, keepsLook, linkText, rememberedName, renderKey, stateIndex } from "../view.js";

/** Every action instance, so a state change repaints all keys. */
const instances = new Set<RbsKeyAction>();
/** What each visible key last got, so unchanged pictures are not sent again. */
const shown = new Map<string, string>();
const known = new Map<string, { settings: Settings; action: WillAppearEvent["action"] }>();
const lastError = new Map<string, string>();
/** Keys whose user set a Stream Deck title: the app draws it, so the picture has no text. */
const titles = new Map<string, Map<number, boolean>>();
const stateOf = new Map<string, number>();
const busy = new Set<string>();
/** Keys pressed while RBS is not reachable: they show the link state until their timer ends. */
const notices = new Map<string, ReturnType<typeof setTimeout>>();
let tick: ReturnType<typeof setInterval> | undefined;

function repaintAll(fromTick = false): void {
	const now = Date.now();
	for (const [id, { settings, action }] of known) {
		const kind = [...instances].find((i) => i.actions.some((a) => a.id === id))?.kind;
		if (!kind) continue;
		const state = client.state as unknown as ViewState;
		const ready = client.status.state === "ready";
		if (ready) {
			endNotice(id);
			const name = rememberedName(kind, settings, state);
			if (name) {
				const next = { ...settings, ...name };
				known.set(id, { settings: next, action });
				void action.setSettings(next as JsonObject);
			}
		}
		const notice = notices.has(id) || !keepsLook(client.status);
		if (action.isDial()) {
			const v = dialView(kind, settings, state);
			const fb = ready ? dialFeedback(v.title, v.volume, v.muted) : notice ? { title: linkText(client.status).title, value: "", indicator: { value: 0 } } : dialFeedback(v.title, undefined, undefined);
			const key = JSON.stringify(fb);
			if (shown.get(id) !== key) {
				shown.set(id, key);
				void action.setFeedback(fb);
			}
			continue;
		}
		if (!action.isKey()) continue;
		// Two-state keys: the user may give each state its own title (and Stream Deck draws it).
		const idx = ready ? stateIndex(kind, settings, state) : 0;
		if (stateOf.get(id) !== idx) {
			stateOf.set(id, idx);
			void action.setState(idx);
		}
		const svg = toDataUrl(renderKey(kind, settings, client.status, state, { elapsed, liveSince: live.since, now, titled: [...(titles.get(id)?.values() ?? [])].some(Boolean) }, notice));
		if (shown.get(id) === svg) continue;
		shown.set(id, svg);
		void action.setImage(svg);
	}
	updateTick();
	if (!fromTick) void sendLists();
}

/**
 * A press while RBS is not reachable: the key shows the link state for `NOTICE_MS`, then its usual
 * look again. A timer exists only while a notice shows (P13).
 */
function showNotice(id: string): void {
	endNotice(id);
	notices.set(id, setTimeout(() => {
		notices.delete(id);
		shown.delete(id);
		repaintAll(true);
	}, NOTICE_MS));
	repaintAll(true);
}

function endNotice(id: string): void {
	const t = notices.get(id);
	if (t === undefined) return;
	clearTimeout(t);
	notices.delete(id);
}

/** The 1 s timer (times, blinking) exists only while a recording or stream runs and a key shows it (P13). */
function updateTick(): void {
	const shows = (kind: Kind) => [...instances].some((i) => i.kind === kind && [...i.actions].length > 0);
	const need = (elapsed.running && (shows("record") || shows("status"))) || (live.on && (shows("stream") || shows("status")));
	if (need && !tick) tick = setInterval(() => repaintAll(true), 1000);
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
	const s = client.state as unknown as ViewState;
	const link = client.status;
	const id = streamDeck.ui.action.id;
	const err = lastError.get(id);
	const status =
		link.state === "ready"
			? `Connected to ${link.server ?? "RBS"} (protocol ${link.protocol})${link.scopes?.includes("control") ? "" : ". This token is view-only: keys will not work."}`
			: link.state === "unreachable"
				? "Not connected. Is RBS running with Settings, Remote control, Allow remote control on?"
				: linkText(link).title + ".";
	const level = link.state === "ready" ? (link.scopes?.includes("control") ? "ok" : "warn") : link.state === "connecting" || link.state === "idle" ? "warn" : "bad";
	await streamDeck.ui.sendToPropertyInspector({ event: "status", status, level, error: err ?? "" });
	await streamDeck.ui.sendToPropertyInspector({ event: "getScenes", items: items(s.scenes?.scenes ?? []) });
	// Every source of the collection, not only the scene on air: a key for a source that is
	// not in the scene on air does nothing until it is.
	const all = items(s.scenes?.sources ?? s.sources ?? []);
	await streamDeck.ui.sendToPropertyInspector({ event: "getSources", items: all });
	await streamDeck.ui.sendToPropertyInspector({ event: "getCollections", items: items(s.collections?.entries ?? []) });
	await streamDeck.ui.sendToPropertyInspector({ event: "getProfiles", items: items(s.profileLibrary?.entries ?? []) });
	await streamDeck.ui.sendToPropertyInspector({ event: "getBuses", items: (s.buses?.buses ?? []).map((b) => ({ label: b.name, value: b.name })) });
	// Filters of the sources in the scene on air (the only ones RBS lists with their filters).
	const filters = (s.sources ?? []).flatMap((src) => (src.filters ?? []).map((f) => ({ label: `${src.name}: ${filterName(f.type)}`, value: String(f.id) })));
	await streamDeck.ui.sendToPropertyInspector({ event: "getFilters", items: filters });
	await streamDeck.ui.sendToPropertyInspector({ event: "getMuteTargets", items: [{ label: "Master", value: "master" }, ...all] });
}

client.onStatus(() => repaintAll());
client.onState(() => repaintAll());

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
		titles.delete(ev.action.id);
		stateOf.delete(ev.action.id);
		endNotice(ev.action.id);
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
		const m = titles.get(ev.action.id) ?? new Map<number, boolean>();
		// A title the user hid with "Show title" off is not drawn by Stream Deck, so it does not count.
		m.set(ev.payload.state ?? 0, Boolean(ev.payload.title) && ev.payload.titleParameters?.showTitle !== false);
		titles.set(ev.action.id, m);
		// Keep a title the user typed in Stream Deck as this key's own text, so it is not lost when they clear it.
		const k = known.get(ev.action.id);
		const typed = ev.payload.title?.trim();
		if (k && typed && ev.payload.titleParameters?.showTitle !== false && !k.settings.label0?.trim()) {
			k.settings = { ...k.settings, label0: typed };
			void ev.action.setSettings(k.settings as JsonObject);
		}
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

	/** True when the link is not ready; the press then asks for a retry and the key says why nothing happened. */
	private notReady(action: { id: string; showAlert(): Promise<void> }): boolean {
		if (client.status.state === "ready") return false;
		client.retryNow();
		if (keepsLook(client.status)) showNotice(action.id);
		else void action.showAlert();
		return true;
	}

	override async onDialRotate(ev: DialRotateEvent<JsonObject>): Promise<void> {
		if (this.notReady(ev.action)) return;
		try {
			await turnVolume(this.kind, ev.payload.settings as Settings, ev.payload.ticks, client);
		} catch (err) {
			lastError.set(ev.action.id, explain(err));
		}
	}

	override async onDialDown(ev: DialDownEvent<JsonObject>): Promise<void> {
		if (this.notReady(ev.action)) return;
		try {
			await press(this.kind === "bus-volume" ? "bus-volume" : "volume", ev.payload.settings as Settings, client);
		} catch (err) {
			lastError.set(ev.action.id, explain(err));
		}
	}

	override async onKeyDown(ev: KeyDownEvent<JsonObject>): Promise<void> {
		const id = ev.action.id;
		if (busy.has(id)) return;
		if (this.notReady(ev.action)) return;
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
