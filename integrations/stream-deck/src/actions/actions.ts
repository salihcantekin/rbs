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
