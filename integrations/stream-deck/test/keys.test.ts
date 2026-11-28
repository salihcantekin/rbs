import { describe, expect, it } from "vitest";

import { Lease } from "../src/lease.js";
import { fit } from "../src/pictures/pictures.js";
import { ManualClock } from "../src/rbs-client/clock.js";
import { RbsClient } from "../src/rbs-client/client.js";
import { positionOf, volumeAt, volumeText } from "../src/fader.js";
import { press, turnVolume } from "../src/press.js";
import { type Elapsed, dialView, filterName, formatElapsed, jobDone, keepsLook, look, rememberedName, renderKey, restingLook, stateIndex, stepTarget, type ViewState } from "../src/view.js";
import { FakeServer } from "./fake-server.js";

const ready = { state: "ready" } as const;
const noElapsed: Elapsed = { ms: 0, at: 0, running: false };

describe("Lease", () => {
	it("connects on the first key and closes after the grace period", () => {
		const clock = new ManualClock();
		const log: string[] = [];
		const lease = new Lease(clock, () => log.push("first"), () => log.push("idle"), 10_000);
		lease.acquire("a");
		lease.acquire("b");
		lease.release("a");
		expect(clock.pending).toBe(0);
		lease.release("b");
		clock.advance(9_999);
		expect(log).toEqual(["first"]);
		clock.advance(1);
		expect(log).toEqual(["first", "idle"]);
	});
	it("a key that appears within the grace period cancels the close", () => {
		const clock = new ManualClock();
		const log: string[] = [];
		const lease = new Lease(clock, () => log.push("first"), () => log.push("idle"));
		lease.acquire("a");
		lease.release("a");
		clock.advance(5_000);
		lease.acquire("a");
		clock.advance(60_000);
		expect(log).toEqual(["first"]);
	});
});

describe("pictures", () => {
	it("fits text to two short lines", () => {
		expect(fit("Main camera wide")).toEqual(["Main camera", "wide"]);
		expect(fit("one two three four five six").length).toBeLessThanOrEqual(2);
	});
});

describe("renderKey", () => {
	const state: ViewState = {
		scenes: { scenes: [{ id: 1, name: "Main" }, { id: 2, name: "Chat" }], program: 1, master: { muted: true } },
		sources: [
			{ id: 7, name: "Camera", item: 70, visible: false, muted: true },
			{ id: 8, name: "Mic", muted: false },
		],
		record: { state: "recording" },
		stream: { state: "live", connected: true },
	};
	const svg = (kind: Parameters<typeof renderKey>[0], settings: object, s = state, link: Parameters<typeof renderKey>[2] = ready) =>
		renderKey(kind, settings, link, s, { elapsed: noElapsed, liveSince: 0, now: 0, titled: false });

	it("highlights the program scene only", () => {
		const c = { elapsed: noElapsed, liveSince: 0, now: 0, titled: false };
		expect(look("scene", { scene: "1" }, state, c).tone).toBe("accent");
		expect(look("scene", { scene: "2" }, state, c).tone).toBe("neutral");
		expect(svg("scene", { scene: "9" })).toContain("Missing");
	});
	it("two-state keys report their state index", () => {
		expect(stateIndex("record", {}, state)).toBe(1);
		expect(stateIndex("scene", { scene: "1" }, state)).toBe(1);
		expect(stateIndex("scene", { scene: "2" }, state)).toBe(0);
		expect(stateIndex("source-visible", { source: "7" }, state)).toBe(1); // hidden
		expect(stateIndex("mute", { source: "7" }, state)).toBe(1); // muted
		expect(stateIndex("mute", { source: "master" }, state)).toBe(1);
		expect(stateIndex("marker", {}, state)).toBe(0);
	});
	it("a key set to one job labels it so", () => {
		const c = { elapsed: noElapsed, liveSince: 0, now: 0, titled: false };
		expect(look("record", { mode: "on" }, { ...state, record: { state: "idle" } }, c).label).toBe("Record");
		expect(look("record", { mode: "on" }, state, c).label).toBe("Recording");
		expect(look("record", { mode: "off" }, { ...state, record: { state: "idle" } }, c)).toMatchObject({ label: "Stop", tone: "dim" });
		expect(look("record-pause", { mode: "off" }, state, c).tone).toBe("dim");
	});
	it("own texts: one per state when toggling, one for a one-job key, which goes dim when done", () => {
		const c = { elapsed: noElapsed, liveSince: 0, now: 0, titled: false };
		const idle = { ...state, record: { state: "idle" as const } };
		const own = { label0: "Start Rec", label1: "Stop" };
		expect(look("record", own, idle, c).label).toBe("Start Rec");
		expect(look("record", own, state, c).label).toBe("Stop");
		expect(look("record", { ...own, mode: "on" }, idle, c)).toMatchObject({ label: "Start Rec", tone: "neutral" });
		expect(look("record", { ...own, mode: "on" }, state, c)).toMatchObject({ label: "Start Rec", tone: "dim", ring: false, pulse: false });
		expect(look("record", { ...own, mode: "off" }, state, c)).toMatchObject({ label: "Start Rec", tone: "live" });
		expect(look("record", { ...own, mode: "off" }, idle, c).tone).toBe("dim");
		expect(look("record", own, state, { ...c, titled: true }).label).toBe("Stop");
	});
	it("every one-job key is passive exactly when its job is done", () => {
		const idle = { ...state, record: { state: "idle" as const }, stream: { state: "idle" as const } };
		expect(jobDone("record", {}, state, "on")).toBe(true);
		expect(jobDone("record", {}, idle, "on")).toBe(false);
		expect(jobDone("stream", {}, state, "on")).toBe(true);
		expect(jobDone("stream", {}, idle, "off")).toBe(true);
		expect(jobDone("record-pause", {}, { ...state, record: { state: "recording", paused: true } }, "on")).toBe(true);
		expect(jobDone("record-pause", {}, state, "off")).toBe(true);
		expect(jobDone("source-visible", { source: "7" }, state, "off")).toBe(true); // hidden
		expect(jobDone("mute", { source: "7" }, state, "on")).toBe(true);
		expect(jobDone("mute", { source: "7" }, state, "off")).toBe(false);
		expect(jobDone("vcam", {}, state, "on")).toBe(false);
		expect(jobDone("studio", {}, state, "off")).toBe(true);
		expect(jobDone("marker", {}, state, "on")).toBe(false);
	});
	it("keeps the usual look while RBS is not reachable, and shows the link state only on a press", () => {
		const ctx = { elapsed: noElapsed, liveSince: 0, now: 0, titled: false };
		const off = { state: "unreachable" } as const;
		const scene = renderKey("scene", { scene: "1", sceneName: "Main" }, off, {}, ctx);
		expect(scene).toContain(">Main<");
		expect(scene).not.toContain("connected");
		expect(renderKey("scene", { scene: "1" }, { state: "connecting" }, {}, ctx)).toContain(">Scene<");
		expect(renderKey("record", {}, off, {}, ctx)).toContain(">Record<");
		expect(renderKey("source-visible", { source: "7", sourceName: "Camera" }, off, {}, ctx)).not.toContain("Not in scene");
		expect(renderKey("mute", { source: "8", label0: "My mic" }, off, {}, ctx)).toContain(">My mic<");
		expect(renderKey("scene", { scene: "1", sceneName: "Main" }, off, {}, ctx, true)).toContain(">connected<");
		expect(renderKey("scene", { scene: "1" }, { state: "idle" }, {}, ctx, true)).toContain(">connected<");
	});
	it("a resting key has no live state", () => {
		const k = restingLook("stream", { mode: "on" }, { elapsed: noElapsed, liveSince: 0, now: 0, titled: false });
		expect(k).toMatchObject({ tone: "neutral", ring: false, dot: false, sub: undefined });
	});
	it("remembers the names a key shows while RBS is not reachable", () => {
		const libs: ViewState = { ...state, collections: { entries: [{ id: "c1", name: "Show" }], current: "c1" } };
		expect(rememberedName("scene", { scene: "2" }, state)).toEqual({ sceneName: "Chat" });
		expect(rememberedName("scene", { scene: "2", sceneName: "Chat" }, state)).toBeUndefined();
		expect(rememberedName("mute", { source: "8" }, state)).toEqual({ sourceName: "Mic" });
		expect(rememberedName("mute", { source: "master" }, state)).toBeUndefined();
		expect(rememberedName("collection", { entry: "c1" }, libs)).toEqual({ sourceName: "Show" });
		expect(rememberedName("scene", { scene: "9" }, state)).toBeUndefined();
	});
	it("keeps showing link states the user must fix", () => {
		expect(keepsLook({ state: "unreachable" })).toBe(true);
		expect(keepsLook({ state: "auth_failed" })).toBe(false);
		expect(svg("record", {}, state, { state: "incompatible", detail: "server_too_old" })).toContain(">Update RBS<");
		expect(svg("stream", {}, state, { state: "no_token" })).toContain(">Set token<");
	});
	it("record and stream follow their state", () => {
		expect(svg("record", {})).toContain(">Stop<");
		expect(svg("record", {}, { ...state, record: { state: "idle" } })).toContain("Record");
		expect(svg("stream", {})).toContain(">Live<");
		expect(svg("stream", {}, { ...state, stream: { state: "live", connected: false, connects: 2 } })).toContain(">Reconnecting<");
	});
	it("a source outside the program scene is marked", () => {
		expect(svg("source-visible", { source: "8" })).toContain("Not in scene");
		expect(svg("source-visible", { source: "7" })).toContain("Camera");
	});
	it("elapsed time advances while recording runs", () => {
		const out = renderKey("record", {}, ready, state, { elapsed: { ms: 61_000, at: 1000, running: true }, liveSince: 0, now: 4000, titled: false });
		expect(out).toContain("01:04");
		expect(formatElapsed(3_725_000)).toBe("1:02:05");
	});
});

describe("press modes", () => {
	it("a start-only key does nothing while recording, a stop-only key nothing while idle", async () => {
		const server = await FakeServer.start();
		server.options.answers["record.status"] = { seq: { "record.state": 1 }, data: { state: "recording" } };
		const client = new RbsClient({ clock: new ManualClock() });
		client.configure(server.port, "rbs1_good");
		client.start();
		await new Promise<void>((r) => client.onStatus((st) => st.state === "ready" && r()));
		const before = server.requests.length;
		await press("record", { mode: "on" }, client);
		expect(server.requests.length).toBe(before);
		await press("record", { mode: "off" }, client);
		expect(server.requests.slice(-1)[0]?.method).toBe("record.stop");
		await press("record-pause", { mode: "on" }, client);
		expect(server.requests.slice(-1)[0]?.method).toBe("record.pause");
		client.stop();
		await server.stop();
	});
});

describe("press", () => {
	it("sends the call each action means", async () => {
		const server = await FakeServer.start();
		server.options.answers["scene.list"] = { seq: { "scene.changed": 1 }, data: { scenes: [{ id: 1, name: "Main" }], program: 1, master: { muted: false }, sources: [] } };
		server.options.answers["source.list"] = { seq: { "source.changed": 1 }, data: [{ id: 7, name: "Camera", item: 70, visible: true, muted: false }] };
		const client = new RbsClient({ clock: new ManualClock() });
		client.configure(server.port, "rbs1_good");
		client.start();
		await new Promise<void>((r) => client.onStatus((s) => s.state === "ready" && r()));
		const calls = () => server.requests.slice(-1)[0];
		await press("scene", { scene: "1" }, client);
		expect(calls()).toEqual({ method: "scene.switch", params: { scene: 1 } });
		await press("record", {}, client);
		expect(calls()).toEqual({ method: "record.start", params: { profile: "p1" } });
		await press("record-pause", {}, client); // not recording: nothing is sent
		expect(calls()?.method).toBe("record.start");
		await press("stream", {}, client);
		expect(calls()?.method).toBe("stream.start");
		await press("source-visible", { source: "7" }, client);
		expect(calls()).toEqual({ method: "item.set_visible", params: { item: 70, visible: false } });
		await press("mute", { source: "7" }, client);
		expect(calls()).toEqual({ method: "source.set_muted", params: { source: 7, muted: true } });
		await press("mute", { source: "master" }, client);
		expect(calls()).toEqual({ method: "canvas.set_master_muted", params: { muted: true } });
		await expect(press("scene", {}, client)).rejects.toThrow(/Choose a scene/);
		client.stop();
		await server.stop();
	});
});

describe("more keys", () => {
	const st: ViewState = {
		scenes: { scenes: [{ id: 1, name: "Main" }, { id: 2, name: "Chat" }], program: 1, preview: 1 },
		sources: [{ id: 7, name: "Camera", item: 1, filters: [{ id: 31, type: "org.rbs.filter.background-removal", enabled: true }] }],
		record: { state: "idle" },
		stream: { state: "idle" },
		replay: { armed: false },
		buses: { buses: [{ name: "Music", volume: 0.5, muted: false }] },
		collections: { entries: [{ id: "a", name: "Show" }, { id: "b", name: "Talk" }], current: "a" },
		profileLibrary: { entries: [{ id: "p", name: "Stream" }], current: "p" },
	};
	const c = { elapsed: { ms: 0, at: 0, running: false }, liveSince: 0, now: 0, titled: false };
	it("looks", () => {
		expect(look("collection", { entry: "a" }, st, c)).toMatchObject({ label: "Show", tone: "accent", sub: "In use" });
		expect(look("collection", { entry: "b" }, st, c)).toMatchObject({ label: "Talk", tone: "neutral" });
		expect(look("collection", { entry: "zz" }, st, c).sub).toBe("Not found");
		expect(look("profile", {}, st, c).sub).toBe("Choose one");
		expect(look("preview", { scene: "2" }, st, c)).toMatchObject({ label: "Chat", sub: "Preview", tone: "neutral" });
		expect(look("preview", { scene: "2" }, { ...st, scenes: { ...st.scenes!, preview: undefined } }, c)).toMatchObject({ sub: "Studio off", tone: "dim" });
		expect(look("filter", { filter: "31" }, st, c)).toMatchObject({ label: "Background removal", tone: "accent", sub: undefined });
		expect(look("filter", { filter: "99" }, st, c).sub).toBe("Not in scene");
		expect(look("replay-buffer", {}, st, c).label).toBe("Replay off");
		expect(look("bus-mute", { bus: "Music" }, st, c)).toMatchObject({ label: "Music", tone: "neutral" });
		expect(look("status", {}, st, c)).toMatchObject({ label: "Idle", tone: "dim" });
		expect(look("status", {}, { ...st, record: { state: "recording" }, stream: { state: "live", connected: true } }, c).label).toBe("Rec + Live");
		expect(look("go-live", {}, st, c).label).toBe("Go live");
		expect(look("go-live", {}, { ...st, stream: { state: "live" } }, c).label).toBe("End all");
	});
	it("names a filter from its type id", () => {
		expect(filterName("org.rbs.filter.color-correction")).toBe("Color correction");
		expect(filterName("blur")).toBe("Blur");
	});
	it("two-state indexes and one-job dimming", () => {
		expect(stateIndex("collection", { entry: "a" }, st)).toBe(1);
		expect(stateIndex("replay-buffer", {}, st)).toBe(0);
		expect(stateIndex("filter", { filter: "31" }, st)).toBe(0);
		expect(stateIndex("bus-mute", { bus: "Music" }, st)).toBe(0);
		expect(jobDone("replay-buffer", {}, st, "off")).toBe(true);
		expect(jobDone("filter", { filter: "31" }, st, "on")).toBe(true);
		expect(jobDone("go-live", {}, st, "off")).toBe(true);
		expect(jobDone("bus-mute", { bus: "Music" }, st, "on")).toBe(false);
		expect(look("filter", { filter: "31", mode: "on" }, st, c).tone).toBe("dim");
	});
	it("reads a track dial", () => {
		expect(dialView("bus-volume", { bus: "Music" }, st)).toMatchObject({ title: "Music", volume: 0.5, muted: false });
		expect(dialView("bus-volume", { bus: "Nope" }, st).volume).toBeUndefined();
	});
});

describe("presses of the more keys", () => {
	it("send the calls", async () => {
		const server = await FakeServer.start();
		server.options.answers["source.list"] = { seq: { "source.changed": 1 }, data: [{ id: 7, name: "Camera", item: 1, filters: [{ id: 31, type: "x", enabled: true }] }] };
		server.options.answers["replay.status"] = { seq: { "replay.changed": 1 }, data: { armed: false } };
		server.options.answers["bus.list"] = { buses: [{ name: "Music", volume: 0.5, muted: false }] };
		server.options.answers["collection.list"] = { entries: [{ id: "a", name: "Show" }, { id: "b", name: "Talk" }], current: "a", pending: false };
		server.options.answers["profile.list"] = { entries: [{ id: "p1", name: "High" }], current: "p1", pending: false };
		const client = new RbsClient({ clock: new ManualClock() });
		client.configure(server.port, "rbs1_good");
		client.start();
		await new Promise<void>((r) => client.onStatus((s) => s.state === "ready" && r()));
		const last = () => server.requests.slice(-1)[0];
		await press("replay-buffer", {}, client);
		expect(last()?.method).toBe("replay.arm");
		await press("collection", { entry: "b" }, client);
		expect(last()?.method).toBe("collection.switch");
		expect(last()?.params).toEqual({ id: "b" });
		const n = server.requests.length;
		await press("collection", { entry: "a" }, client); // in use: nothing
		expect(server.requests.length).toBe(n);
		await press("filter", { filter: "31" }, client);
		expect(last()).toEqual({ method: "filter.set_enabled", params: { filter: 31, enabled: false } });
		await press("bus-mute", { bus: "Music" }, client);
		expect(last()).toEqual({ method: "bus.set", params: { name: "Music", muted: true } });
		await turnVolume("bus-volume", { bus: "Music", step: "10" }, -1, client);
		expect(last()?.method).toBe("bus.set");
		await press("go-live", {}, client);
		expect(last()?.method).toBe("stream.start");
		expect(server.requests.some((r) => r.method === "record.start")).toBe(true);
		await expect(press("bus-mute", {}, client)).rejects.toThrow(/Choose a track/);
		client.stop();
		await server.stop();
	});
});

describe("scene steps and dials", () => {
	const state: ViewState = {
		scenes: { scenes: [{ id: 1, name: "A" }, { id: 2, name: "B", active: false }, { id: 3, name: "C" }], program: 3, master: { volume: 0.5 } },
		sources: [{ id: 7, name: "Mic", volume: 0.8, muted: false }],
	};
	it("steps over inactive scenes and wraps", () => {
		expect(stepTarget(state, "next")?.id).toBe(1);
		// in studio mode it steps from the preview scene
		expect(stepTarget({ ...state, scenes: { ...state.scenes!, preview: 1 } }, "next")?.id).toBe(3);
		expect(stepTarget(state, "previous")?.id).toBe(1);
		expect(stepTarget({ scenes: { scenes: [{ id: 1, name: "A" }], program: 1 } }, "next")).toBeUndefined();
	});
	it("reads a dial's source", () => {
		expect(dialView("volume", { source: "7" }, state)).toMatchObject({ title: "Mic", volume: 0.8 });
		expect(dialView("volume", { source: "master" }, state)).toMatchObject({ title: "Master", volume: 0.5 });
		expect(dialView("volume", { source: "99" }, state).volume).toBeUndefined();
	});
	it("turns volume in steps and clamps", async () => {
		const server = await FakeServer.start();
		server.options.answers["source.list"] = { seq: { "source.changed": 1 }, data: [{ id: 7, name: "Mic", item: 1, volume: 0.8 }] };
		const client = new RbsClient({ clock: new ManualClock() });
		client.configure(server.port, "rbs1_good");
		client.start();
		await new Promise<void>((r) => client.onStatus((st) => st.state === "ready" && r()));
		// 0.8 is at fader position 0.968 (-1.9 dB); two steps of 10 percent down is position 0.768 (-13.9 dB)
		await turnVolume("volume", { source: "7", step: "10" }, -2, client);
		const sent = (server.requests.slice(-1)[0]?.params as { volume: number }).volume;
		expect(20 * Math.log10(sent)).toBeCloseTo(-13.94, 1);
		expect(positionOf(sent)).toBeCloseTo(0.7677, 2);
		await turnVolume("volume", { source: "7", step: "25" }, 5, client);
		expect(server.requests.slice(-1)[0]?.params).toEqual({ source: 7, volume: 1 });
		client.stop();
		await server.stop();
	});
});

describe("fader scale", () => {
	it("matches RBS's fader: linear in dB from -60 to 0, the left end is silence", () => {
		expect(positionOf(1)).toBe(1);
		expect(positionOf(0)).toBe(0);
		expect(positionOf(10 ** (-30 / 20))).toBeCloseTo(0.5, 5);
		expect(volumeAt(0.5)).toBeCloseTo(10 ** (-30 / 20), 5);
		expect(volumeAt(0)).toBe(0);
		expect(volumeText(10 ** (-6 / 20))).toBe("-6.0 dB");
		expect(volumeText(0)).toBe("Silent");
	});
	it("next scene moves the preview in studio mode", async () => {
		const server = await FakeServer.start();
		server.options.answers["scene.list"] = { seq: { "scene.changed": 1 }, data: { scenes: [{ id: 1, name: "A" }, { id: 2, name: "B" }], program: 1, preview: 1, sources: [] } };
		const client = new RbsClient({ clock: new ManualClock() });
		client.configure(server.port, "rbs1_good");
		client.start();
		await new Promise<void>((r) => client.onStatus((st) => st.state === "ready" && r()));
		await press("scene-step", { direction: "next" }, client);
		expect(server.requests.slice(-1)[0]).toEqual({ method: "canvas.set_preview", params: { scene: 2 } });
		client.stop();
		await server.stop();
	});
});
