# RBS Stream Deck plugin

Controls RBS from an Elgato Stream Deck: switch scene, record, stream, show or hide a source, mute. Keys show RBS's state. It is a client of the RBS Control API (spec: [docs/specs/stream-deck.md](../../docs/specs/stream-deck.md), ADR-0198). Needs Stream Deck 7.1 or newer.

## Install a built package

1. In RBS: Settings, Remote control, turn on *Allow remote control*, make a token with the *Control* level (copy it; it is shown once).
2. Double-click `com.rbs.streamdeck.streamDeckPlugin`. Stream Deck installs it (category *RBS*).
3. Drag an RBS action onto a key. In its settings enter the port (default 4456) and paste the token once; it is shared by all RBS keys.

## Develop

```sh
npm ci --ignore-scripts
npm test            # schema drift check + vitest
npm run build       # bundles src/plugin.ts into the .sdPlugin folder
npm run pack        # dist/com.rbs.streamdeck.streamDeckPlugin
npx streamdeck link com.rbs.streamdeck.sdPlugin   # load it into Stream Deck for development
npm run gen-types   # after a schema change in crates/rbs-api/schemas
```
