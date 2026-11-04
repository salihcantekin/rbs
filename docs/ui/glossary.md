# Glossary of user-facing terms

UI text uses these terms and no synonyms (rule U13). Code may use different internal names; the mapping is listed.

| Term in UI | Meaning | Not | Internal name |
| --- | --- | --- | --- |
| Scene | A layout of sources shown on a canvas | Layout, page | `Scene` |
| Source | Something that produces video or audio (screen capture, window capture, camera, image, text, media, browser, audio device), as placed in a scene with its position, size and crop. The panel listing them is "Sources" | Input, layer, element, item | Source node; placement is a scene item |
| Filter | An effect applied to a source or audio | Effect, plugin | Filter node |
| Transition | How the view changes from one scene to another | Effect | Transition node |
| Canvas | An output picture size and frame rate, for example horizontal 1920x1080 or vertical 1080x1920 | Resolution | Canvas |
| Output | A destination: a streaming service, a recording, a virtual camera | Target | Output node |
| Go live / End stream | Start / stop streaming to the selected outputs | Start stream, Stop live | - |
| Record | Save the program to a file. "Capture" is used only in source type names (Screen capture, Window capture) | Save, Capture (as a verb for recording) | Recording output |
| Preview / Program | What you are preparing / what viewers see (studio mode) | - | - |
| Mixer | The audio panel with levels and controls | Audio panel | Audio graph UI |
| Workspace | A saved arrangement of panels, floating windows and projectors. It never holds scenes, sources or output settings | Profile, layout, scene collection | UI workspace file |
| Scene collection | A set of scenes and sources saved together. It holds no panel layout and no output settings | Project, workspace | Scene document |
| Remove from scene / Delete source | Remove the source from this scene only / remove it from every scene | Delete (for removing from one scene) | Scene item removal / source removal |
| Studio mode | Separate preview and program, with transitions between them | Two-pane mode | - |
| Projector | A full-screen or window view of a canvas, scene or source on any display | External view | - |
| Track | One of the audio mixes written to a recording or sent to an output | Channel | Audio bus output |
| Monitor | Listening to a source locally without sending it to outputs | Cue | Monitoring send |
| Shortcut | A key combination for a command; "global shortcut" when it works outside RBS | Hotkey | - |
| Stinger | A transition that plays a video over the change | Video transition | Stinger transition node |
| Output profile | A saved set of output settings (service, encoder, bitrate, recording format). It holds no scenes and no panel layout | Profile, workspace | Output configuration |
| Remote control | Letting other apps (Stream Deck, panels, scripts) control and watch RBS through the control API. Off by default | API server, WebSocket server | Control API server (`rbs-api`, `rbs-api-ws`) |
| Access token | A secret one app uses for remote control, with a name and an access level (View only, Control, Full control). Shown once; can be revoked | Password, key, API key | `rbs_api::Token` with its scopes |
| Access from other computers | Remote control by apps on other computers on the network, over an encrypted connection. Off by default | Network mode, LAN access, remote access | Network access, TLS (`WsConfig::network`, ADR-0046) |
| Key pin | What apps on other computers check before they send an access token: `sha256//` and the hash of RBS's public key. Stays the same when the certificate is renewed | Fingerprint, thumbprint | SPKI SHA-256 pin (`TlsIdentity::key_pin`) |
