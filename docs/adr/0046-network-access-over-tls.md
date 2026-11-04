# ADR-0046: Network access to the control API: TLS on the remote control port, a generated key the client pins

- Status: Accepted
- Date: 2026-11-04
- Deciders: Salih (owner)

## Context

[ADR-0044](0044-control-api-protocol-and-security.md) decision 11 keeps the control API on loopback unless the user turns on network access, which is a separate setting. Every connection that is not loopback must then use TLS. Decision 9 lets a client send its bearer token only over loopback, IPC, or TLS whose certificate the client verified, through a CA or a pinned fingerprint. The [control API spec](../specs/control-api.md) leaves the certificate open. Salih chose (2026-11-04): RBS makes its own certificate, the remote app pins its fingerprint once, and the user may load their own certificate instead.

What exists today:

- `rbs-api-ws` serves WebSocket on loopback only. It has its own RFC 6455 framing and one I/O thread on the OS readiness poller (`polling`), with non-blocking sockets. It refuses a non-loopback address at `start`.
  - Its accept path counts every connection that is still upgrading against one cap (16), and every socket against another (128), without telling loopback and remote peers apart.
  - It applies no per-address limit before `TransportInbox::connect`.
- `rbs-api` already separates loopback and remote peers once a connection reaches it:
  - `PeerInfo`;
  - `ScopeSet::for_channel` (`plugins` and `tokens` never remote, `admin` only with `allow_remote_admin`);
  - slots reserved for local peers;
  - pending-handshake and per-address (/64) caps;
  - authentication back-off per address;
  - the `Host` check in `UpgradePolicy`.
- `rbs-app` has the Remote control settings (`rbs-remote-settings`, `remote-control.toml`): on or off, the port, and access tokens.

Who connects from another computer:

- A Stream Deck or Companion host on the network.
- A script or a user-built app on another machine.
- A browser panel on a tablet. Browsers cannot pin a certificate: a user who clicks through a warning about a self-signed certificate has not verified anything.

Forces:

- **Never send the token to an unverified server** (ADR-0044 decision 9).
- **Zero cost when unused** (ADR-0013). With network access off, no TLS state, certificate or extra listener may exist.
- **Layers** (P1). TLS code lives in the adapter. `rbs-api` names no TLS library. `rbs-remote-settings` stays free of crypto dependencies.
- **Remote peers must not lock out local ones** (ADR-0044, spec "Exposure and limits"), at any layer.
- **Licences and builds** (ADR-0010, MSRV 1.87). No extra system tools for the supported targets.

## Decision

1. **TLS library: `rustls` with the `ring` provider, in `rbs-api-ws` only.**
   - The `ServerConnection` does no I/O of its own. The existing I/O thread passes socket bytes through `read_tls`, `process_new_packets` and `write_tls`, and the WebSocket code reads and writes plaintext through it. No thread is added.
   - Dependencies:
     - `rustls`: `default-features = false`, features `ring`, `std`, `tls12`, `logging` off.
     - `rcgen`: `default-features = false`, features `ring`, `pem`.
     - `socket2` (decision 3).
   - Each version is chosen with a `rust-version` of 1.87 or lower.
   - `deny.toml` bans `aws-lc-sys` and `openssl-sys`.
   - Licences: rustls is Apache-2.0, ISC or MIT; ring is Apache-2.0 AND ISC; rcgen and socket2 are MIT or Apache-2.0. All are already allowed.
   - `ring` needs only the C compiler the workspace needs anyway. On `aarch64-pc-windows-msvc` that is clang.
   - TLS 1.2 and 1.3 with rustls' default suites, and ALPN `http/1.1`. No TLS 1.3 session tickets (`send_tls13_tickets = 0`) and no session cache: there are few clients, and the server keeps less state.
   - The I/O loop must handle two sans-I/O cases:
     - It drains plaintext that rustls already decrypted (`reader()`) before it sleeps. This applies above all when a paused connection resumes, because buffered plaintext raises no readiness event.
     - It registers writable interest whenever `wants_write()` is true. Alerts and the handshake write without any WebSocket output.
     - A paused connection stops calling `read_tls`, not only draining, because `read_tls` fails once rustls' plaintext buffer is full.
     - Every write goes through the TLS session: the upgrade answer, refusals, WebSocket frames and closes. `send_close_notify` comes before the write shutdown.

2. **One port, the protocol detected per connection, and remote traffic accounted separately.**
   - The first byte of a connection decides its protocol. `0x16` (a TLS handshake record) starts TLS. Anything else is plain HTTP, and plain HTTP is accepted only from a loopback peer.
   - A plain request from a non-loopback peer gets a fixed `426 Upgrade Required`, whose body says to use `wss://`. The connection is then closed, and nothing the peer sent is used.
   - A peer is loopback when the address the OS reports, canonicalised (IPv4-mapped IPv6 to IPv4), is a loopback address. Headers never decide this.
   - Loopback clients keep `ws://localhost:<port>`, and may also use `wss://`.
   - In the transport, connections that are still upgrading (TLS handshake or HTTP upgrade) and sockets are counted per class, loopback and remote:
     - Remote connections still upgrading have their own cap (16).
     - A per-address cap (8 per address, IPv6 per /64) applies at accept, before any TLS work.
     - 32 of the 128 sockets are reserved for loopback.
   - So a remote flood, even one renewed every upgrade deadline, cannot lock out local clients. This mirrors the reserve `rbs-api` keeps for local sessions.

3. **Listening addresses.** With network access on, the transport listens on one dual-stack socket, `[::]` with `IPV6_V6ONLY` off. The flag is set through `socket2`, because std cannot set it before bind, and Windows defaults to v6-only. Where IPv6 is unavailable, it listens on `0.0.0.0`. With network access off it binds `127.0.0.1` and `::1` as today.

4. **The key and certificate.**
   - **Generated (the default).** When the user first turns network access on, RBS makes:
     - an ECDSA P-256 key;
     - a self-signed certificate for it, with `CA:false`, the `serverAuth` extended key usage, and a validity of 825 days, which is Apple's maximum for TLS server certificates.
     - The certificate's subject alternative names are `localhost`, the host name, `<host name>.local`, and the addresses of the interfaces at that moment.
   - **What clients pin: the key.** Clients pin the SHA-256 of the key's SubjectPublicKeyInfo, shown as `sha256//<base64>`, the form curl's `--pinnedpubkey` uses.
     - Settings also shows the SHA-256 fingerprint of the certificate (colon-separated hex), for clients that can only compare that.
     - Pinning the key means a renewed certificate keeps the same pin. A certificate whose addresses changed (DHCP) also keeps it.
     - RBS renews the certificate with the same key when it starts with fewer than 30 days of validity left.
     - "New key" in Settings replaces the key: every client must pin again, and connected remote sessions are closed.
   - **Storage.** `rbs-api-ws` generates the key and certificate and computes both fingerprints (module `tls`). `rbs-remote-settings` stores them as PEM next to `remote-control.toml`, in `remote-control-cert.pem` and `remote-control-key.pem`, without any crypto dependency.
     - The key file is created with mode 0600 on Unix. It is not created first and changed after.
     - On Windows, the files go in the per-user application data folder, which is user-only.
     - Under the settings lock, the key is written first and the certificate second, each by an atomic rename. At load, a key that does not match the certificate is reported like a missing file.
     - The certificate's expiry date is stored in `remote-control.toml`, so renewal needs no certificate parser.
     - A key file that others can read or write is refused (Unix).
   - **Renewal.** At renewal the certificate fingerprint changes, but the key pin does not. Settings marks the certificate fingerprint as renewed, the renewal is logged, and the text recommends the key pin.
   - **Never regenerated silently.** If the key or certificate is missing or unreadable while network access is on, network access does not start, and Settings says so. It offers "New key" and warns that clients must pin again. So a changed fingerprint is always something the user did.
   - **The user's own.** Settings can point to a PEM certificate chain and key instead, for example from a CA the user runs. RBS reads them each time network access starts, never copies them, and on Unix refuses a key others can read. They are not watched while network access runs. A certificate that expires within 30 days, read at start, is shown in Settings and logged.
   - **Browser panels.** A browser on another device cannot pin, so it needs a certificate that device already trusts: the user's own. RBS does not offer to install its generated certificate on other devices in this version. Settings says browser panels need the user's own certificate.
   - **Clients.** ADR-0044 decision 9 applies: a client sends its token only after the pin or the CA verified the certificate, and never after a mismatch. No RBS client library exists yet. When one is written (TypeScript first, for Stream Deck plugins), it must take the `sha256//` pin and report a mismatch as its own error, never as an authentication failure.
   - `rbs-cli remote fingerprint` prints both fingerprints from the stored certificate, for headless setups. It reads the files and opens no connection.

5. **The `Host` check and channel facts.**
   - `UpgradePolicy` accepts:
     - loopback names;
     - the host name and `<host name>.local`, added automatically;
     - host names the user adds;
     - the local address of the accepted socket (`getsockname`, canonicalised).
   - It never accepts an unspecified address (`0.0.0.0`, `::`).
   - `UpgradePolicy::check` gains the connection's local address. This is an `rbs-api` API change; the workspace crates are not published.
   - `PeerInfo` gains `secure` (TLS). `rbs-api` refuses a non-loopback WebSocket peer that is not secure. This is defence in depth, so a transport bug cannot open a plain remote session.
   - `Origin` and `Host` checks run after TLS and before the upgrade completes, unchanged. The TLS handshake is part of the upgrade deadline (10 s).

6. **Memory, stated honestly.**
   - Before authentication, queued output is capped at 16 KiB, so pongs a peer never reads cannot grow it. The rustls buffer limit is set to 16 KiB (`set_buffer_limit`).
   - During the handshake, rustls may buffer handshake messages of up to about 64 KiB.
   - The 4 KiB pre-authentication message limit stays at the WebSocket layer, where messages are skipped as they arrive.
   - The bound is about 100 KiB per socket, about 13 MiB at the 128-socket cap. After authentication the existing per-connection output bound (1 MiB) applies as today.

7. **Settings, visibility and logging.**
   - "Allow access from other computers" sits under Remote control. It is off by default and needs remote control on.
   - When on, the section shows:
     - the `wss://` address;
     - both fingerprints;
     - "Allow remote admin", off by default. Remote sessions get `read` and `control`, plus `admin` only with this setting; `plugins` and `tokens` never;
     - "New key";
     - the choice of the user's own certificate.
   - While RBS is reachable from the network, the main window shows a persistent indicator, outside the Settings window.
   - Settings tells the user:
     - to turn network access on only on networks they trust;
     - that Windows and macOS ask for firewall permission the first time;
     - that a refused firewall prompt blocks other computers without any further message.
   - Every non-loopback connection is logged with its address and TLS version, as well as its authentication result (ADR-0044).
   - The new settings are stored in `remote-control.toml` as new fields with defaults, so existing files still load.

8. **Unsupported: local proxies and tunnels.** A reverse proxy or tunnel on the same computer (SSH `-R`, `tailscale serve`, `cloudflared`, Caddy) makes remote users look like loopback peers. They would then get the local-only scopes and loopback limits. That setup is unsupported and documented as such. `X-Forwarded-*` and similar headers are never honoured.

9. **Zero cost.**
   - Off: nothing changes from today. The transport binds loopback, and no TLS configuration, certificate, key or `socket2` socket exists.
   - On: the TLS configuration is built once at start. Per connection, TLS state exists only for connections that start TLS. The I/O thread still sleeps until the OS reports activity.

## Consequences

- Stream Deck and Companion hosts, scripts and user-built apps on other computers can control RBS without the user running a CA. The token never leaves the computer in clear.
- Browser panels on other devices need the user's own certificate for now.
- `rbs-api-ws` gains `rustls`, `ring` (through rustls), `rcgen` and `socket2`. It becomes the workspace's only TLS stack.
- `rbs-api` gains `PeerInfo::secure` and a local-address argument on `UpgradePolicy::check`.
- `rbs-remote-settings` gains PEM file storage. `rbs-cli` gains `remote fingerprint`.
- `docs/specs/control-api.md` is updated with the implementation:
  - the `Host` rule (the local address of the accepted socket);
  - the TLS dependency row;
  - the transport caps and the loopback reserve;
  - `PeerInfo::secure`;
  - the network access settings.
- Tests:
  - The transport conformance suite runs a second time over TLS.
  - A plain request from a remote peer gets 426.
  - A TLS handshake that never finishes hits the deadline.
  - A byte-at-a-time TLS client works.
  - A flood of remote handshakes does not lock out loopback.
  - IPv4-mapped loopback is classified as loopback.
  - Plain `ws://` over loopback still works with network access on.
  - A paused TLS connection resumes with plaintext already decrypted.
  - A connection with nothing to send still sends its TLS 1.3 tickets.
  - Fingerprint test vectors in both forms.
  - The key file mode.
  - "Off builds no TLS configuration".
  - Renewal keeps the pin.
  - A client with the wrong pin never sends its token (test client).
  - A refused upgrade over TLS returns a readable 403.
  - `rbs-api` refuses an insecure remote WebSocket peer.
- Users pin once per client, and again after "New key". The Settings text and the docs explain it.
- No mutual TLS. Tokens remain the client credential (ADR-0044); mTLS stays a later option.
- The Settings section and the main-window indicator follow the UI rules (U1–U21, design system, glossary) and get their own UI review.

## Alternatives considered

- **Only a user-supplied certificate.** Most users have no CA, so network access would stay unused or be bypassed. Kept as an option, and needed for browser panels.
- **Install the generated certificate on other devices** (a small local CA, exported from Settings). It would serve browser panels. But it adds a CA key that can sign for any name unless name-constrained, and a per-device install flow. Left for later.
- **Pin the certificate instead of the key.** That is simpler to explain, but every renewal or address change would break all clients.
- **A separate TLS port.** Two ports to explain and to open in firewalls. The first byte of TLS and of HTTP never collide, so one port is enough.
- **Two listeners, `0.0.0.0` and `[::]`.** On Linux and macOS `[::]` is dual-stack by default, so the second bind fails. One dual-stack socket set through `socket2` behaves the same everywhere.
- **Plain `ws://` on the network with challenge-response authentication.** It protects the token, but not the commands, events or window titles that follow, and it allows tampering. ADR-0044 already chose TLS.
- **`native-tls` / OpenSSL / SChannel / Secure Transport.** Three stacks with different behaviour, and OpenSSL is a system dependency on Linux.
- **The `aws-lc-rs` provider** (rustls' default). It needs CMake, and NASM on Windows.
- **An async runtime with tokio-rustls.** It adds threads and dependencies the sans-I/O API makes unnecessary.
- **Choosing which interfaces to listen on.** This would help a laptop that moves to an untrusted Wi-Fi network. But interfaces and their addresses change while RBS runs, and a per-interface list is hard to explain. The persistent indicator and the trusted-network warning serve that case for now; it can be added later.
- **ACME certificates.** They need a public domain name pointing at the studio network.

## Review

Independent review per ADR-0014:

- **Round 1, independent AI session** (a fresh subagent given only the ADR, the specs and the code; it was started from the author's session, which the owner may weigh), 2026-11-04. Verdict: changes requested. Each concern and how it was resolved:
  - Browser panels cannot pin: decision 4, "Browser panels", and the new alternative on installing the generated certificate.
  - The transport counts loopback peers against the upgrade cap and has no per-address cap at accept: decision 2.
  - The `0.0.0.0` and `[::]` binds conflict: decision 3.
  - The pre-authentication memory claim was wrong: decision 6.
  - Sans-I/O stalls (buffered plaintext, writes that rustls needs): decision 1.
  - The `Host` check would refuse the advertised name and could accept unspecified addresses: decision 5.
  - Pin the key, not the certificate: decision 4.
  - Validity over 825 days: decision 4.
  - Key file creation, atomic pair and no silent regeneration: decision 4.
  - `PeerInfo::secure`: decision 5.
  - Local proxies and tunnels: decision 8.
  - Dependency features, bans and MSRV: decision 1.
  - Missing tests: Consequences.
  - `rbs-cli` and client libraries: decision 4.
  - Exposure UX and logging: decision 7.
  - The 426 body: decision 2.
- **Round 2, independent AI session** (fresh subagent, same caveat), 2026-11-04. Verdict: approve with changes. All round-1 concerns were found resolved. Each new concern and how it was resolved:
  - The memory bound was too low: decision 6, with an output cap before authentication.
  - Renewal changes the certificate fingerprint: decision 4, "Renewal".
  - Spec sync: Consequences.
  - The loopback reserve was not a number: decision 2.
  - Two files cannot be written atomically as a pair: decision 4, "Storage".
  - The expiry date for renewal: decision 4, "Storage".
  - Every write must go through TLS: decision 1.
  - No session tickets: decision 1.
  - The user's own certificate is not re-read: decision 4.
  - No interface selection: Alternatives.
- Status set to Accepted by the thread under the project's ADR policy, after Salih chose the approach (generated certificate, client pins, own certificate optional) on 2026-11-04.
