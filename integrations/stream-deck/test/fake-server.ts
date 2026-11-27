import { type AddressInfo } from "node:net";
import { WebSocketServer, type WebSocket } from "ws";

export interface FakeOptions {
	protocol?: string;
	token?: string;
	/** Answers per method; default answers are filled in. */
	answers?: Record<string, unknown>;
}

/** An in-process Control API server on an ephemeral loopback port. */
export class FakeServer {
	readonly wss: WebSocketServer;
	readonly requests: { method: string; params: unknown }[] = [];
	readonly sockets = new Set<WebSocket>();
	options: Required<Omit<FakeOptions, "answers">> & { answers: Record<string, unknown> };

	private constructor(wss: WebSocketServer, options: FakeOptions) {
		this.wss = wss;
		this.options = { protocol: "1.115", token: "rbs1_good", answers: {}, ...options };
		wss.on("connection", (ws, req) => {
			this.sockets.add(ws);
			this.origin = req.headers.origin;
			this.subprotocol = ws.protocol;
			ws.on("close", () => this.sockets.delete(ws));
			ws.on("message", (raw) => this.handle(ws, JSON.parse(raw.toString())));
		});
	}

	origin: string | undefined;
	subprotocol = "";

	static async start(options: FakeOptions = {}): Promise<FakeServer> {
		const wss = new WebSocketServer({ host: "127.0.0.1", port: 0, handleProtocols: (p) => (p.has("rbs.v1") ? "rbs.v1" : false) });
		await new Promise<void>((r) => wss.once("listening", () => r()));
		return new FakeServer(wss, options);
	}

	get port(): number {
		return (this.wss.address() as AddressInfo).port;
	}

	async stop(): Promise<void> {
		for (const s of this.sockets) s.terminate();
		await new Promise<void>((r) => this.wss.close(() => r()));
	}

	/** Sends an event to every client. */
	event(topic: string, seq: number, data: unknown): void {
		for (const s of this.sockets) s.send(JSON.stringify({ jsonrpc: "2.0", method: "event", params: { topic, seq, data } }));
	}

	private reply(ws: WebSocket, id: number, result: unknown): void {
		ws.send(JSON.stringify({ jsonrpc: "2.0", id, result }));
	}

	private fail(ws: WebSocket, id: number, code: number, kind: string, reason?: string): void {
		ws.send(JSON.stringify({ jsonrpc: "2.0", id, error: { code, message: kind, data: { kind, reason } } }));
	}

	private handle(ws: WebSocket, msg: { id: number; method: string; params: Record<string, unknown> }): void {
		this.requests.push({ method: msg.method, params: msg.params });
		const o = this.options;
		const custom = o.answers[msg.method];
		if (custom !== undefined) {
			if (typeof custom === "function") return void (custom as (r: (v: unknown) => void, e: (c: number, k: string, r?: string) => void) => void)((v) => this.reply(ws, msg.id, v), (c, k, r) => this.fail(ws, msg.id, c, k, r));
			return this.reply(ws, msg.id, custom);
		}
		switch (msg.method) {
			case "session.hello":
				return this.reply(ws, msg.id, { protocol: o.protocol, instance: "i1" });
			case "session.authenticate":
				if (msg.params.token !== o.token) {
					this.fail(ws, msg.id, -32002, "auth_failed");
					return void setTimeout(() => ws.close(4002, "auth_failed"), 0);
				}
				return this.reply(ws, msg.id, { scopes: ["read", "control"], irreversible: true, server: { name: "rbs", version: "0.1.0" } });
			case "events.subscribe":
				return this.reply(ws, msg.id, { seq: {} });
			case "scene.list":
				return this.reply(ws, msg.id, { seq: { "scene.changed": 5 }, data: { scenes: [{ id: 1, name: "Main" }, { id: 2, name: "Chat" }], program: 1, sources: [] } });
			case "source.list":
				return this.reply(ws, msg.id, { seq: { "source.changed": 3 }, data: [] });
			case "record.status":
				return this.reply(ws, msg.id, { seq: { "record.state": 2 }, data: { state: "idle", last: null } });
			case "stream.status":
				return this.reply(ws, msg.id, { seq: { "stream.state": 2 }, data: { state: "idle", last: null, available: true } });
			case "output.profiles":
				return this.reply(ws, msg.id, { profiles: [{ id: "p1", name: "High" }] });
			default:
				return this.reply(ws, msg.id, {});
		}
	}
}
