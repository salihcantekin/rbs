import { describe, expect, it } from "vitest";

import { Lease } from "../src/lease.js";
import { fit } from "../src/pictures/pictures.js";
import { ManualClock } from "../src/rbs-client/clock.js";
import { RbsClient } from "../src/rbs-client/client.js";
import { press, turnVolume } from "../src/press.js";
import { type Elapsed, dialView, formatElapsed, renderKey, stepTarget, type ViewState } from "../src/view.js";
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
		expect(svg("scene", { scene: "1" })).toContain("#16355f");
		expect(svg("scene", { scene: "2" })).not.toContain("#16355f");
		expect(svg("scene", { scene: "9" })).toContain("Missing");
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
		await turnVolume({ source: "7", step: "5" }, 2, client);
		expect(server.requests.slice(-1)[0]).toEqual({ method: "source.set_volume", params: { source: 7, volume: 0.9 } });
		await turnVolume({ source: "7", step: "25" }, 5, client);
		expect(server.requests.slice(-1)[0]?.params).toEqual({ source: 7, volume: 1 });
		client.stop();
		await server.stop();
	});
});
