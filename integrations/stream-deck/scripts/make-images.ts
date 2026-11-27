// Writes the default key images (the idle pictures) and the plugin icon source.
// Run: npx vite-node scripts/make-images.ts
import { writeFileSync } from "node:fs";
import { pauseKey, muteKey, recordKey, sceneKey, streamKey, visibleKey } from "../src/pictures/pictures.js";

const dir = "com.rbs.streamdeck.sdPlugin/imgs/";
writeFileSync(dir + "scene-key.svg", sceneKey("Scene", false, false));
writeFileSync(dir + "record-key.svg", recordKey("idle"));
writeFileSync(dir + "record-pause-key.svg", pauseKey("idle"));
writeFileSync(dir + "stream-key.svg", streamKey("idle"));
writeFileSync(dir + "source-visible-key.svg", visibleKey("Source", true));
writeFileSync(dir + "mute-key.svg", muteKey("Mute", false));
