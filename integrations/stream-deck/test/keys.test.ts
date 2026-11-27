import { describe, expect, it } from "vitest";

import { Lease } from "../src/lease.js";
import { fit } from "../src/pictures/pictures.js";
import { ManualClock } from "../src/rbs-client/clock.js";
import { RbsClient } from "../src/rbs-client/client.js";
import { positionOf, volumeAt, volumeText } from "../src/fader.js";
import { press, turnVolume } from "../src/press.js";
import { type Elapsed, dialView, formatElapsed, jobDone, look, renderKey, stateIndex, stepTarget, type ViewState } from "../src/view.js";
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
	it("shows the link state instead of RBS state when not ready", () => {
		expect(svg("scene", { scene: "1" }, state, { state: "unreachable" })).toContain(">connected<");
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
		expect(dialView({ source: "7" }, state)).toMatchObject({ title: "Mic", volume: 0.8 });
		expect(dialView({ source: "master" }, state)).toMatchObject({ title: "Master", volume: 0.5 });
		expect(dialView({ source: "99" }, state).volume).toBeUndefined();
	});
	it("turns volume in steps and clamps", async () => {
		const server = await FakeServer.start();
		server.options.answers["source.list"] = { seq: { "source.changed": 1 }, data: [{ id: 7, name: "Mic", item: 1, volume: 0.8 }] };
		const client = new RbsClient({ clock: new ManualClock() });
		client.configure(server.port, "rbs1_good");
		client.start();
		await new Promise<void>((r) => client.onStatus((st) => st.state === "ready" && r()));
		// 0.8 is at fader position 0.968 (-1.9 dB); two steps of 10 percent down is position 0.768 (-13.9 dB)
		await turnVolume({ source: "7", step: "10" }, -2, client);
		const sent = (server.requests.slice(-1)[0]?.params as { volume: number }).volume;
		expect(20 * Math.log10(sent)).toBeCloseTo(-13.94, 1);
		expect(positionOf(sent)).toBeCloseTo(0.7677, 2);
		await turnVolume({ source: "7", step: "25" }, 5, client);
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
