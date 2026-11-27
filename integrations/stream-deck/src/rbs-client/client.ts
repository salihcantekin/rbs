import WebSocket from "ws";

import { type Clock, systemClock } from "./clock.js";
import type { Methods } from "./generated.js";
import type { OutputProfilesResult, RecordStatusResult, SceneListResult, SourceListResult, StreamStatusResult } from "./generated.js";
import { CLOSE, LinkError, RbsError, SUBPROTOCOL, isLoopbackHost, versionVerdict } from "./protocol.js";

/** What the plugin shows on its keys and in the inspector (docs/specs/stream-deck.md, Connection). */
export type LinkState = "idle" | "connecting" | "ready" | "no_token" | "unreachable" | "auth_failed" | "revoked" | "incompatible";

export interface LinkStatus {
	state: LinkState;
	/** For `incompatible`: which side must be updated. */
	detail?: "server_too_old" | "plugin_too_old";
	/** The server's protocol version while `ready`, or the one that did not fit. */
	protocol?: string;
	/** The server's name and version while `ready`. */
	server?: string;
	scopes?: string[];
	irreversible?: boolean;
}

export type SceneListData = SceneListResult.Root["data"];
export type SourceListData = SourceListResult.Root["data"];
export type RecordStateData = Extract<RecordStatusResult.Root, { data: unknown }>["data"];
export type StreamStateData = Extract<StreamStatusResult.Root, { data: unknown }>["data"];
export type ProfileData = OutputProfilesResult.Root["profiles"];

/** The latest state of the four subscribed topics, plus the recording profiles. */
export interface RbsState {
	scenes?: SceneListData;
	sources?: SourceListData;
	record?: RecordStateData;
	stream?: StreamStateData;
	profiles?: ProfileData;
}

export type StateKey = "scenes" | "sources" | "record" | "stream";

const TOPICS: Record<string, StateKey> = {
	"scene.changed": "scenes",
	"source.changed": "sources",
	"record.state": "record",
	"stream.state": "stream",
};

const CLIENT_NAME = "rbs-stream-deck";
const REQUEST_TIMEOUT_MS = 10_000;
const CONNECT_TIMEOUT_MS = 15_000;
const MAX_IN_FLIGHT = 8;
const BACKOFF_FIRST_MS = 1000;
const BACKOFF_MAX_MS = 15_000;

/** The part of a WebSocket the client uses; tests may pass their own. */
export interface SocketLike {
	on(event: "open", fn: () => void): unknown;
	on(event: "message", fn: (data: { toString(): string }) => void): unknown;
	on(event: "close", fn: (code: number) => void): unknown;
	on(event: "error", fn: (err: Error) => void): unknown;
	send(data: string): void;
	close(code?: number): void;
}

export interface ClientOptions {
	clock?: Clock;
	/** Plugin version, sent in `session.hello`. */
	version?: string;
	/** Host to connect to. Only loopback hosts are accepted (ADR-0198 decision 3). */
	host?: string;
	/** Makes the socket; tests replace it. */
	connectSocket?: (url: string) => SocketLike;
}

interface Pending {
	resolve: (v: unknown) => void;
	reject: (e: unknown) => void;
	cancel: () => void;
}

/**
 * A Control API client (protocol 1, docs/specs/control-api.md). It owns one connection:
 * handshake, request and answer matching, subscriptions with `seq` filtering, reconnect
 * with back-off. It imports nothing from Stream Deck.
 */
export class RbsClient {
	private readonly clock: Clock;
	private readonly host: string;
	private readonly version: string;
	private readonly connectSocket: (url: string) => SocketLike;

	private port = 4456;
	private token = "";
	private running = false;
	private generation = 0;
	private socket?: SocketLike;
	private nextId = 1;
	private pending = new Map<number, Pending>();
	private attempt = 0;
	private cancelRetry?: () => void;
	private cancelConnectTimer?: () => void;
	/** Set when the handshake ended the link for good (until settings change). */
	private terminal?: LinkStatus;

	private held: Partial<Record<StateKey, number>> = {};
	private current: LinkStatus = { state: "idle" };
	private data: RbsState = {};
	private statusListeners = new Set<(s: LinkStatus) => void>();
	private stateListeners = new Set<(s: RbsState, changed: StateKey | "profiles") => void>();

	constructor(options: ClientOptions = {}) {
		this.clock = options.clock ?? systemClock;
		this.host = options.host ?? "127.0.0.1";
		this.version = options.version ?? "0.0.0";
		if (!isLoopbackHost(this.host)) {
			throw new LinkError("closed", `refusing to send a token to ${this.host}: only loopback hosts are allowed`);
		}
		this.connectSocket =
			options.connectSocket ?? ((url) => new WebSocket(url, [SUBPROTOCOL], { maxPayload: 8 * 1024 * 1024 }) as unknown as SocketLike);
	}

	get status(): LinkStatus {
		return this.current;
	}

	get state(): RbsState {
		return this.data;
	}

	onStatus(fn: (s: LinkStatus) => void): () => void {
		this.statusListeners.add(fn);
		return () => void this.statusListeners.delete(fn);
	}

	onState(fn: (s: RbsState, changed: StateKey | "profiles") => void): () => void {
		this.stateListeners.add(fn);
		return () => void this.stateListeners.delete(fn);
	}

	/** Sets the port and token. A change restarts the link, which also ends a terminal state. */
	configure(port: number, token: string): void {
		const changed = port !== this.port || token !== this.token;
		this.port = port;
		this.token = token;
		if (!changed) return;
		this.terminal = undefined;
		if (this.running) this.reconnect();
	}

	/** Starts connecting (no-op when running). */
	start(): void {
		if (this.running) return;
		this.running = true;
		this.reconnect();
	}

	/** Closes the connection and stops retrying. */
	stop(): void {
		if (!this.running) return;
		this.running = false;
		this.teardown(CLOSE.normal);
		this.data = {};
		this.held = {};
		this.setStatus({ state: "idle" });
	}

	/** A key press while the link is down: try now instead of waiting for the back-off. */
	retryNow(): void {
		if (this.running && this.current.state === "unreachable") this.reconnect();
	}

	/** Calls a method. Needs `ready`. */
	call<M extends keyof Methods>(method: M, params?: Methods[M]["params"]): Promise<Methods[M]["result"]> {
		if (this.current.state !== "ready") return Promise.reject(new LinkError("not_ready", "not connected to RBS"));
		return this.rpc(method, params) as Promise<Methods[M]["result"]>;
	}

	// ---- internals -------------------------------------------------------------------

	private reconnect(): void {
		this.teardown(CLOSE.normal);
		this.held = {};
		this.data = {};
		if (!this.token) {
			this.setStatus({ state: "no_token" });
			return;
		}
		if (this.terminal) {
			this.setStatus(this.terminal);
			return;
		}
		const gen = ++this.generation;
		this.setStatus({ state: "connecting" });
		let socket: SocketLike;
		try {
			socket = this.connectSocket(`ws://${this.host}:${this.port}`);
		} catch {
			this.scheduleRetry();
			return;
		}
		this.socket = socket;
		this.cancelConnectTimer = this.clock.setTimeout(() => {
			if (gen === this.generation && this.current.state === "connecting") socket.close(CLOSE.normal);
		}, CONNECT_TIMEOUT_MS);
		socket.on("open", () => void this.handshake(gen));
		socket.on("message", (data) => this.onMessage(gen, data.toString()));
		socket.on("close", (code) => this.onClose(gen, code));
		socket.on("error", () => {
			/* a close follows */
		});
	}

	private teardown(code: number): void {
		this.generation++;
		this.cancelRetry?.();
		this.cancelRetry = undefined;
		this.cancelConnectTimer?.();
		this.cancelConnectTimer = undefined;
		const socket = this.socket;
		this.socket = undefined;
		this.failPending(new LinkError("closed", "connection closed"));
		try {
			socket?.close(code);
		} catch {
			/* already closed */
		}
	}

	private failPending(err: unknown): void {
		const all = [...this.pending.values()];
		this.pending.clear();
		for (const p of all) {
			p.cancel();
			p.reject(err);
		}
	}

	private async handshake(gen: number): Promise<void> {
		try {
			const hello = (await this.rpc("session.hello", {
				protocols: ["1.0"],
				client: { name: CLIENT_NAME, version: this.version },
			})) as { protocol: string };
			if (gen !== this.generation) return;
			const verdict = versionVerdict(hello.protocol);
			if (verdict !== "ok") return this.end({ state: "incompatible", detail: verdict, protocol: hello.protocol });

			const auth = (await this.rpc("session.authenticate", { token: this.token })) as {
				scopes: string[];
				irreversible?: boolean;
				server: { name: string; version: string };
			};
			if (gen !== this.generation) return;
			await this.rpc("events.subscribe", { topics: Object.keys(TOPICS) as never });
			const [scenes, sources, record, stream, profiles] = await Promise.all([
				this.rpc("scene.list", {}),
				this.rpc("source.list", {}),
				this.rpc("record.status", {}),
				this.rpc("stream.status", {}),
				this.rpc("output.profiles", {}),
			]);
			if (gen !== this.generation) return;
			this.applySnapshot("scenes", scenes as never);
			this.applySnapshot("sources", sources as never);
			this.applySnapshot("record", record as never);
			this.applySnapshot("stream", stream as never);
			this.data = { ...this.data, profiles: (profiles as { profiles: ProfileData }).profiles };
			this.cancelConnectTimer?.();
			this.cancelConnectTimer = undefined;
			this.attempt = 0;
			this.setStatus({
				state: "ready",
				protocol: hello.protocol,
				server: `${auth.server.name} ${auth.server.version}`,
				scopes: auth.scopes,
				irreversible: auth.irreversible ?? false,
			});
		} catch (err) {
			if (gen !== this.generation) return;
			if (err instanceof RbsError) {
				if (err.kind === "auth_failed") return this.end({ state: "auth_failed" });
				if (err.kind === "unsupported_protocol") return this.end({ state: "incompatible", detail: "plugin_too_old" });
			}
			// Anything else during the handshake (closed, timeout, a refusal): try again later.
			this.socket?.close(CLOSE.normal);
		}
	}

	/** The handshake ended the link for good: stay in `status` until the settings change. */
	private end(status: LinkStatus): void {
		this.terminal = status;
		this.teardown(CLOSE.normal);
		this.setStatus(status);
	}

	private onClose(gen: number, code: number): void {
		if (gen !== this.generation) return;
		this.cancelConnectTimer?.();
		this.cancelConnectTimer = undefined;
		this.socket = undefined;
		this.failPending(new LinkError("closed", "connection closed"));
		this.held = {};
		this.data = {};
		if (code === CLOSE.authFailed) return this.terminate({ state: "auth_failed" });
		if (code === CLOSE.revoked) return this.terminate({ state: "revoked" });
		if (code === CLOSE.unsupportedProtocol) return this.terminate({ state: "incompatible", detail: "plugin_too_old" });
		this.scheduleRetry();
	}

	private terminate(status: LinkStatus): void {
		this.terminal = status;
		this.generation++;
		this.setStatus(status);
	}

	private scheduleRetry(): void {
		if (!this.running) return;
		this.setStatus({ state: "unreachable" });
		const delay = Math.min(BACKOFF_FIRST_MS * 2 ** this.attempt, BACKOFF_MAX_MS);
		this.attempt++;
		this.cancelRetry?.();
		this.cancelRetry = this.clock.setTimeout(() => {
			this.cancelRetry = undefined;
			if (this.running) this.reconnect();
		}, delay);
	}

	private rpc(method: string, params: unknown): Promise<unknown> {
		const socket = this.socket;
		if (!socket) return Promise.reject(new LinkError("closed", "not connected"));
		if (this.pending.size >= MAX_IN_FLIGHT) return Promise.reject(new LinkError("too_many", "too many requests in flight"));
		const id = this.nextId++;
		return new Promise((resolve, reject) => {
			const cancel = this.clock.setTimeout(() => {
				this.pending.delete(id);
				reject(new LinkError("timeout", `${method} timed out`));
			}, REQUEST_TIMEOUT_MS);
			this.pending.set(id, { resolve, reject, cancel });
			try {
				socket.send(JSON.stringify({ jsonrpc: "2.0", id, method, params }));
			} catch (e) {
				this.pending.delete(id);
				cancel();
				reject(new LinkError("closed", String(e)));
			}
		});
	}

	private onMessage(gen: number, text: string): void {
		if (gen !== this.generation) return;
		let msg: {
			id?: number;
			method?: string;
			result?: unknown;
			error?: { code: number; message: string; data?: { kind?: string; reason?: string } };
			params?: { topic: string; seq: number; data: unknown };
		};
		try {
			msg = JSON.parse(text);
		} catch {
			return;
		}
		if (msg.method === "event" && msg.params) return this.onEvent(msg.params.topic, msg.params.seq, msg.params.data);
		if (typeof msg.id !== "number") return;
		const p = this.pending.get(msg.id);
		if (!p) return;
		this.pending.delete(msg.id);
		p.cancel();
		if (msg.error) {
			const d = msg.error.data;
			p.reject(new RbsError(d?.kind ?? "unknown", msg.error.code, msg.error.message, d?.reason));
		} else p.resolve(msg.result);
	}

	private onEvent(topic: string, seq: number, data: unknown): void {
		const key = TOPICS[topic];
		if (!key) return;
		const have = this.held[key];
		if (have !== undefined && seq <= have) return;
		this.held[key] = seq;
		this.data = { ...this.data, [key]: data };
		this.emitState(key);
	}

	private applySnapshot(key: StateKey, answer: { seq?: Record<string, number>; data: unknown }): void {
		const topic = Object.keys(TOPICS).find((t) => TOPICS[t] === key) as string;
		const seq = answer.seq?.[topic];
		const have = this.held[key];
		if (seq !== undefined && have !== undefined && seq <= have) return; // an event was newer
		if (seq !== undefined) this.held[key] = seq;
		this.data = { ...this.data, [key]: answer.data };
	}

	private setStatus(s: LinkStatus): void {
		this.current = s;
		for (const fn of [...this.statusListeners]) fn(s);
		if (s.state === "ready") for (const fn of [...this.stateListeners]) fn(this.data, "profiles");
	}

	private emitState(key: StateKey): void {
		for (const fn of [...this.stateListeners]) fn(this.data, key);
	}
}
