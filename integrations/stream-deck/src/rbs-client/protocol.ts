// Control API protocol constants and errors (docs/specs/control-api.md, ADR-0198).

/** The protocol major this client speaks. */
export const PROTOCOL_MAJOR = 1;
/** Lowest server minor the plugin needs (scenes and streaming, protocol 1.6). */
export const MIN_MINOR = 6;
/** WebSocket subprotocol of protocol major 1. */
export const SUBPROTOCOL = "rbs.v1";

/** Close codes of the Control API's WebSocket transport. */
export const CLOSE = {
	normal: 1000,
	goingAway: 1001,
	handshakeTimeout: 4001,
	authFailed: 4002,
	unsupportedProtocol: 4003,
	tooManyConnections: 4004,
	revoked: 4005,
	tooSlow: 4006,
} as const;

/** An error answer of the server (`error.data.kind` is the stable name). */
export class RbsError extends Error {
	constructor(
		/** `data.kind`, for example `scope_denied`; `unknown` when the server sent none. */
		readonly kind: string,
		readonly code: number,
		message: string,
		/** `data.reason` when the server sent one (for example `not_permitted` reasons). */
		readonly reason?: string,
	) {
		super(message);
		this.name = "RbsError";
	}
}

/** A failure of the link itself, not an answer of the server. */
export class LinkError extends Error {
	constructor(readonly kind: "closed" | "timeout" | "not_ready" | "too_many", message: string) {
		super(message);
		this.name = "LinkError";
	}
}

/** Parses `major.minor`; `undefined` when it is not that. */
export function parseVersion(text: string): { major: number; minor: number } | undefined {
	const m = /^(\d+)\.(\d+)/.exec(text);
	return m ? { major: Number(m[1]), minor: Number(m[2]) } : undefined;
}

/** Whether a server's protocol version can be used by this client. */
export function versionVerdict(text: string): "ok" | "server_too_old" | "plugin_too_old" {
	const v = parseVersion(text);
	if (!v) return "plugin_too_old";
	if (v.major > PROTOCOL_MAJOR) return "plugin_too_old";
	if (v.major < PROTOCOL_MAJOR || v.minor < MIN_MINOR) return "server_too_old";
	return "ok";
}

/** Whether `host` is a loopback name or address: the only place a token is sent. */
export function isLoopbackHost(host: string): boolean {
	const h = host.replace(/^\[|\]$/g, "").toLowerCase();
	return h === "localhost" || h === "::1" || /^127(\.\d{1,3}){3}$/.test(h);
}
