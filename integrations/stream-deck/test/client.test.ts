import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { RbsClient, type LinkStatus } from "../src/rbs-client/client.js";
import { ManualClock } from "../src/rbs-client/clock.js";
import { LinkError, RbsError, isLoopbackHost, versionVerdict } from "../src/rbs-client/protocol.js";
import { FakeServer } from "./fake-server.js";

/** Resolves when the client reaches a link state (no sleeping). */
function reaches(c: RbsClient, state: LinkStatus["state"]): Promise<LinkStatus> {
	if (c.status.state === state) return Promise.resolve(c.status);
	return new Promise((resolve) => {
		const off = c.onStatus((s) => {
			if (s.state === state) {
				off();
				resolve(s);
			}
		});
	});
}

describe("protocol helpers", () => {
	it("judges server versions", () => {
		expect(versionVerdict("1.115")).toBe("ok");
		expect(versionVerdict("1.6")).toBe("ok");
		expect(versionVerdict("1.5")).toBe("server_too_old");
		expect(versionVerdict("2.0")).toBe("plugin_too_old");
		expect(versionVerdict("0.9")).toBe("server_too_old");
	});
	it("knows loopback hosts", () => {
		for (const h of ["127.0.0.1", "localhost", "::1", "[::1]"]) expect(isLoopbackHost(h)).toBe(true);
		for (const h of ["192.168.1.5", "example.com", "0.0.0.0"]) expect(isLoopbackHost(h)).toBe(false);
	});
	it("refuses a token for a non-loopback host", () => {
		expect(() => new RbsClient({ host: "192.168.1.5" })).toThrow(LinkError);
	});
});

describe("RbsClient", () => {
	let server: FakeServer;
	let clock: ManualClock;
	let client: RbsClient;

	beforeEach(async () => {
		server = await FakeServer.start();
		clock = new ManualClock();
		client = new RbsClient({ clock });
	});
	afterEach(async () => {
		client.stop();
		await server.stop();
	});

	it("connects, authenticates, subscribes and loads the state", async () => {
		client.configure(server.port, "rbs1_good");
		client.start();
		const s = await reaches(client, "ready");
		expect(s.protocol).toBe("1.115");
		expect(s.scopes).toEqual(["read", "control"]);
		expect(server.subprotocol).toBe("rbs.v1");
		expect(server.origin).toBeUndefined();
		expect(client.state.scenes?.program).toBe(1);
		expect(client.state.profiles?.[0]?.id).toBe("p1");
		const sub = server.requests.find((r) => r.method === "events.subscribe");
		expect(sub?.params).toEqual({ topics: ["scene.changed", "source.changed", "record.state", "stream.state"] });
		expect(server.requests[0]?.method).toBe("session.hello");
	});

	it("shows no_token without connecting", () => {
		client.start();
		expect(client.status.state).toBe("no_token");
		expect(server.requests).toHaveLength(0);
	});

	it("stops retrying after a wrong token until the token changes", async () => {
		client.configure(server.port, "rbs1_bad");
		client.start();
		await reaches(client, "auth_failed");
		expect(clock.pending).toBe(0);
		client.configure(server.port, "rbs1_good");
		await reaches(client, "ready");
	});

	it("flags an old server and a newer major", async () => {
		server.options.protocol = "1.3";
		client.configure(server.port, "rbs1_good");
		client.start();
		expect(await reaches(client, "incompatible")).toMatchObject({ detail: "server_too_old", protocol: "1.3" });
		server.options.protocol = "2.0";
		client.configure(server.port, "rbs1_good2");
		expect(await reaches(client, "incompatible")).toMatchObject({ detail: "plugin_too_old" });
	});

	it("applies events only with a higher seq", async () => {
		client.configure(server.port, "rbs1_good");
		client.start();
		await reaches(client, "ready");
		const seen: unknown[] = [];
		client.onState((s) => seen.push(s.record));
		server.event("record.state", 2, { state: "recording" }); // equal to the snapshot: dropped
		server.event("record.state", 4, { state: "recording" });
		server.event("record.state", 3, { state: "idle" }); // older: dropped
		await new Promise<void>((r) => client.onState((s, k) => k === "record" && s.record && r()));
		expect(client.state.record).toEqual({ state: "recording" });
	});

	it("matches answers by id and maps error kinds", async () => {
		server.options.answers["scene.switch"] = (_ok: unknown, err: (c: number, k: string, r?: string) => void) => err(-32012, "not_permitted", "on_air");
		client.configure(server.port, "rbs1_good");
		client.start();
		await reaches(client, "ready");
		await expect(client.call("scene.switch", { scene: 2 })).rejects.toMatchObject({ kind: "not_permitted", reason: "on_air" });
		await expect(client.call("scene.switch", { scene: 2 })).rejects.toBeInstanceOf(RbsError);
		await expect(client.call("stream.start", {})).resolves.toEqual({});
	});

	it("refuses calls before ready", async () => {
		await expect(client.call("scene.list", {})).rejects.toMatchObject({ kind: "not_ready" });
	});

	it("retries with back-off and clears state after the server goes away", async () => {
		client.configure(server.port, "rbs1_good");
		client.start();
		await reaches(client, "ready");
		const port = server.port;
		await server.stop();
		await reaches(client, "unreachable");
		expect(client.state.scenes).toBeUndefined();
		expect(clock.delays).toEqual([1000]);
		clock.advance(1000); // connect attempt fails
		await new Promise<void>((r) => {
			const off = client.onStatus((s) => s.state === "unreachable" && (off(), r()));
		});
		expect(clock.delays).toEqual([2000]);
		// the server comes back on the same port: the next attempt is ready again
		server = await FakeServer.start();
		client.configure(server.port, "rbs1_good");
		expect(port).toBeTypeOf("number");
		await reaches(client, "ready");
	});

	it("fails requests that are in flight when the socket closes", async () => {
		server.options.answers["stream.start"] = () => {
			/* never answers */
		};
		client.configure(server.port, "rbs1_good");
		client.start();
		await reaches(client, "ready");
		const p = client.call("stream.start", {});
		const failed = expect(p).rejects.toMatchObject({ kind: "closed" });
		for (const s of server.sockets) s.terminate();
		await failed;
	});

	it("times out a request on the injected clock", async () => {
		server.options.answers["stream.start"] = () => {};
		client.configure(server.port, "rbs1_good");
		client.start();
		await reaches(client, "ready");
		const p = client.call("stream.start", {});
		const failed = expect(p).rejects.toMatchObject({ kind: "timeout" });
		await new Promise((r) => setImmediate(r));
		clock.advance(10_000);
		await failed;
	});

	it("retryNow reconnects at once while unreachable", async () => {
		client.configure(1, "rbs1_good"); // nothing listens on port 1
		client.start();
		await reaches(client, "unreachable");
		client.configure(1, "rbs1_good");
		client.retryNow();
		expect(client.status.state).toBe("connecting");
	});
});
