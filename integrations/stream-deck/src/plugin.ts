import streamDeck from "@elgato/streamdeck";

import { MarkerAction, MuteAction, RecordAction, RecordPauseAction, ReplayAction, SceneAction, SceneStepAction, SourceVisibleAction, StreamAction, VcamAction, VolumeAction } from "./actions/actions.js";
import { loadGlobalSettings } from "./runtime.js";

streamDeck.actions.registerAction(new SceneAction());
streamDeck.actions.registerAction(new RecordAction());
streamDeck.actions.registerAction(new RecordPauseAction());
streamDeck.actions.registerAction(new StreamAction());
streamDeck.actions.registerAction(new SceneStepAction());
streamDeck.actions.registerAction(new ReplayAction());
streamDeck.actions.registerAction(new VcamAction());
streamDeck.actions.registerAction(new MarkerAction());
streamDeck.actions.registerAction(new VolumeAction());
streamDeck.actions.registerAction(new SourceVisibleAction());
streamDeck.actions.registerAction(new MuteAction());

await streamDeck.connect();
await loadGlobalSettings();
