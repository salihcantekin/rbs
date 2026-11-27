import streamDeck from "@elgato/streamdeck";

import { MuteAction, RecordAction, SceneAction, SourceVisibleAction, StreamAction } from "./actions/actions.js";
import { loadGlobalSettings } from "./runtime.js";

streamDeck.actions.registerAction(new SceneAction());
streamDeck.actions.registerAction(new RecordAction());
streamDeck.actions.registerAction(new StreamAction());
streamDeck.actions.registerAction(new SourceVisibleAction());
streamDeck.actions.registerAction(new MuteAction());

await streamDeck.connect();
await loadGlobalSettings();
