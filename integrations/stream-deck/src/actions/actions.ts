import { action } from "@elgato/streamdeck";

import { PLUGIN_UUID } from "../runtime.js";
import { RbsKeyAction } from "./base.js";

@action({ UUID: `${PLUGIN_UUID}.scene` })
export class SceneAction extends RbsKeyAction {
	readonly kind = "scene" as const;
}

@action({ UUID: `${PLUGIN_UUID}.record` })
export class RecordAction extends RbsKeyAction {
	readonly kind = "record" as const;
}

@action({ UUID: `${PLUGIN_UUID}.record-pause` })
export class RecordPauseAction extends RbsKeyAction {
	readonly kind = "record-pause" as const;
}

@action({ UUID: `${PLUGIN_UUID}.scene-step` })
export class SceneStepAction extends RbsKeyAction {
	readonly kind = "scene-step" as const;
}

@action({ UUID: `${PLUGIN_UUID}.replay` })
export class ReplayAction extends RbsKeyAction {
	readonly kind = "replay" as const;
}

@action({ UUID: `${PLUGIN_UUID}.vcam` })
export class VcamAction extends RbsKeyAction {
	readonly kind = "vcam" as const;
}

@action({ UUID: `${PLUGIN_UUID}.marker` })
export class MarkerAction extends RbsKeyAction {
	readonly kind = "marker" as const;
}

@action({ UUID: `${PLUGIN_UUID}.studio` })
export class StudioAction extends RbsKeyAction {
	readonly kind = "studio" as const;
}

@action({ UUID: `${PLUGIN_UUID}.transition` })
export class TransitionAction extends RbsKeyAction {
	readonly kind = "transition" as const;
}

@action({ UUID: `${PLUGIN_UUID}.volume` })
export class VolumeAction extends RbsKeyAction {
	readonly kind = "volume" as const;
}

@action({ UUID: `${PLUGIN_UUID}.stream` })
export class StreamAction extends RbsKeyAction {
	readonly kind = "stream" as const;
}

@action({ UUID: `${PLUGIN_UUID}.source-visible` })
export class SourceVisibleAction extends RbsKeyAction {
	readonly kind = "source-visible" as const;
}

@action({ UUID: `${PLUGIN_UUID}.mute` })
export class MuteAction extends RbsKeyAction {
	readonly kind = "mute" as const;
}

@action({ UUID: `${PLUGIN_UUID}.replay-buffer` })
export class ReplayBufferAction extends RbsKeyAction {
	readonly kind = "replay-buffer" as const;
}

@action({ UUID: `${PLUGIN_UUID}.collection` })
export class CollectionAction extends RbsKeyAction {
	readonly kind = "collection" as const;
}

@action({ UUID: `${PLUGIN_UUID}.profile` })
export class ProfileAction extends RbsKeyAction {
	readonly kind = "profile" as const;
}

@action({ UUID: `${PLUGIN_UUID}.preview` })
export class PreviewAction extends RbsKeyAction {
	readonly kind = "preview" as const;
}

@action({ UUID: `${PLUGIN_UUID}.status` })
export class StatusAction extends RbsKeyAction {
	readonly kind = "status" as const;
}

@action({ UUID: `${PLUGIN_UUID}.go-live` })
export class GoLiveAction extends RbsKeyAction {
	readonly kind = "go-live" as const;
}

@action({ UUID: `${PLUGIN_UUID}.filter` })
export class FilterAction extends RbsKeyAction {
	readonly kind = "filter" as const;
}

@action({ UUID: `${PLUGIN_UUID}.bus-mute` })
export class BusMuteAction extends RbsKeyAction {
	readonly kind = "bus-mute" as const;
}

@action({ UUID: `${PLUGIN_UUID}.bus-volume` })
export class BusVolumeAction extends RbsKeyAction {
	readonly kind = "bus-volume" as const;
}
