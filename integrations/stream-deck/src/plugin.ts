import streamDeck from "@elgato/streamdeck";

import { MarkerAction, MuteAction, RecordAction, RecordPauseAction, ReplayAction, SceneAction, SceneStepAction, SourceVisibleAction, StreamAction, StudioAction, TransitionAction, VcamAction, VolumeAction, ReplayBufferAction, CollectionAction, ProfileAction, PreviewAction, StatusAction, GoLiveAction, FilterAction, BusMuteAction, BusVolumeAction } from "./actions/actions.js";
import { loadGlobalSettings } from "./runtime.js";

streamDeck.actions.registerAction(new SceneAction());
streamDeck.actions.registerAction(new RecordAction());
streamDeck.actions.registerAction(new RecordPauseAction());
streamDeck.actions.registerAction(new StreamAction());
streamDeck.actions.registerAction(new SceneStepAction());
streamDeck.actions.registerAction(new ReplayAction());
streamDeck.actions.registerAction(new VcamAction());
streamDeck.actions.registerAction(new MarkerAction());
streamDeck.actions.registerAction(new StudioAction());
streamDeck.actions.registerAction(new TransitionAction());
streamDeck.actions.registerAction(new VolumeAction());
streamDeck.actions.registerAction(new SourceVisibleAction());
streamDeck.actions.registerAction(new MuteAction());
streamDeck.actions.registerAction(new ReplayBufferAction());
streamDeck.actions.registerAction(new CollectionAction());
streamDeck.actions.registerAction(new ProfileAction());
streamDeck.actions.registerAction(new PreviewAction());
streamDeck.actions.registerAction(new StatusAction());
streamDeck.actions.registerAction(new GoLiveAction());
streamDeck.actions.registerAction(new FilterAction());
streamDeck.actions.registerAction(new BusMuteAction());
streamDeck.actions.registerAction(new BusVolumeAction());

await streamDeck.connect();
await loadGlobalSettings();
