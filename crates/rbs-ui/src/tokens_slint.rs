//! The Slint side of the design tokens (rule U6, ADR-0057): `tokens.slint` (the `Theme`
//! and `Tokens` globals) and `tokens_apply.rs` (the setters `rbs-app` calls), generated
//! from the token file. Plain text generation, no toolkit dependency. Used by the build
//! scripts of the UI crates (`rbs-app-ui-base`, `rbs-app-ui-single` and the facade
//! `rbs-app-ui`, ADR-0138 decision 3), so both build modes share one generator.

use std::fmt::{self, Write as _};
use std::path::Path;
use std::time::SystemTime;

use crate::tokens::{
    BUTTON_FILLS, BUTTON_SURFACES, TokenError, Tokens, button_outline, panel_outline,
};

/// `color.surface.base` -> `surface-base`; `space.1` -> `space-1`.
fn ident(name: &str, strip: &str) -> String {
    name.strip_prefix(strip).unwrap_or(name).replace('.', "-")
}

/// `color.surface.base` -> `surface_base` (the Rust name Slint gives a field or setter).
fn rust(name: &str, strip: &str) -> String {
    ident(name, strip).replace('-', "_")
}

/// Whether a length, duration or text style can change with the theme (ADR-0057).
fn themeable(name: &str) -> bool {
    crate::tokens::themeable(name).is_some()
}

/// One effect field of a slot: Slint name, Slint type, calm default, and the Rust
/// expression on `crate::tokens::EffectDraw` (`fx`) that fills it.
type EffectField = (String, &'static str, &'static str, String);

/// The effect fields of a slot (ADR-0120, ADR-0122). Every derived colour is computed
/// in Rust; Slint mixes nothing (ADR-0122 decision 4).
fn effect_fields() -> Vec<EffectField> {
    const CLEAR: &str = "#00000000";
    let mut f: Vec<EffectField> = vec![
        ("effects-on".into(), "bool", "false", "fx.on".into()),
        (
            "effects-shadow-blur".into(),
            "length",
            "0px",
            "fx.shadow_blur".into(),
        ),
        (
            "effects-shadow-offset".into(),
            "length",
            "0px",
            "fx.shadow_offset".into(),
        ),
        (
            "effects-shadow".into(),
            "color",
            CLEAR,
            "argb(fx.shadow)".into(),
        ),
        (
            "effects-halo-blur".into(),
            "length",
            "0px",
            "fx.halo_blur".into(),
        ),
        (
            "effects-halo".into(),
            "color",
            CLEAR,
            "argb(fx.halo)".into(),
        ),
        (
            "effects-key-edge".into(),
            "length",
            "0px",
            "fx.key_edge".into(),
        ),
        ("effects-press".into(), "length", "0px", "fx.press".into()),
        ("effects-rim-on".into(), "bool", "false", "fx.rim_on".into()),
        (
            "effects-inner-blur".into(),
            "length",
            "0px",
            "fx.inner_blur".into(),
        ),
        (
            "effects-plate-shadow-blur".into(),
            "length",
            "0px",
            "fx.plate_shadow_blur".into(),
        ),
        (
            "effects-plate-shadow-offset".into(),
            "length",
            "0px",
            "fx.plate_shadow_offset".into(),
        ),
        (
            "effects-plate-key-edge".into(),
            "length",
            "0px",
            "fx.plate_key_edge".into(),
        ),
        (
            "effects-plate-glow-blur".into(),
            "length",
            "0px",
            "fx.plate_glow_blur".into(),
        ),
    ];
    // Per kind, in `crate::tokens::FACE_KINDS` order. Checked and danger have no
    // pressed fill of their own, so they have no pressed stops either; the plate has
    // only its rest stops.
    for (i, (kind, states)) in [
        ("primary", 3),
        ("secondary", 3),
        ("checked", 2),
        ("danger", 2),
        // The chosen plate neither hovers nor presses.
        ("plate", 1),
    ]
    .into_iter()
    .enumerate()
    {
        f.push((
            format!("effects-key-{kind}"),
            "color",
            CLEAR,
            format!("argb(fx.key[{i}])"),
        ));
        // The face is shaded: some stop differs from its fill.
        f.push((
            format!("effects-shade-{kind}"),
            "bool",
            "false",
            format!("!fx.faces[{i}].is_flat()"),
        ));
        // The searched gloss stop, as a fraction of the height.
        f.push((
            format!("effects-stop-{kind}"),
            "float",
            "0",
            format!("fx.faces[{i}].stop"),
        ));
        for (s, state) in ["rest", "hover", "pressed"]
            .into_iter()
            .take(states)
            .enumerate()
        {
            f.push((
                format!("effects-top-{kind}-{state}"),
                "color",
                CLEAR,
                format!("argb(fx.faces[{i}].top[{s}])"),
            ));
            f.push((
                format!("effects-bottom-{kind}-{state}"),
                "color",
                CLEAR,
                format!("argb(fx.faces[{i}].bottom[{s}])"),
            ));
        }
        // Only a secondary or danger button takes a rim and an inner glow.
        if kind == "secondary" || kind == "danger" {
            f.push((
                format!("effects-rim-{kind}"),
                "color",
                CLEAR,
                format!("argb(fx.rim[{i}])"),
            ));
            f.push((
                format!("effects-inner-{kind}"),
                "color",
                CLEAR,
                format!("argb(fx.inner[{i}])"),
            ));
        }
    }
    f
}

/// `tokens.slint`: the `Theme` and `Tokens` globals. Every value has the built-in
/// token file as its default, so the files compile and preview on their own; the app
/// sets them at run time (`tokens_apply.rs`).
///
/// What a theme can change lives in two slots (`Theme.slots`): slot 0 is used while the
/// OS scheme is dark, slot 1 while it is light, and every `Theme.*` colour and themeable
/// `Tokens.*` value reads `slots[active]`, so an OS scheme change needs no Rust code
/// (themes spec, "Applying"). The rest (fixed colours, window sizes, keyboard steps) are
/// single values.
pub fn tokens_slint(t: &Tokens) -> Result<String, std::fmt::Error> {
    let mut s = String::new();
    writeln!(
        s,
        "// Generated by rbs-ui tokens_slint from crates/rbs-ui/tokens.toml. Do not edit."
    )?;
    writeln!(s, "import {{ Palette }} from \"std-widgets.slint\";")?;
    let colors: Vec<&String> = t.themes[0].1.keys().collect();
    let lengths: Vec<&String> = t.length.keys().filter(|n| themeable(n)).collect();
    let durations: Vec<&String> = t.duration.keys().filter(|n| themeable(n)).collect();
    let styles: Vec<&String> = t.type_styles.keys().collect();
    writeln!(s, "// Everything one theme sets.")?;
    writeln!(s, "export struct ThemeSlot {{")?;
    writeln!(s, "    panel-outline: bool,")?;
    writeln!(s, "    button-outline: bool,")?;
    writeln!(s, "    font-family: string,")?;
    writeln!(s, "    halo-clear: length,")?;
    let effects = effect_fields();
    for (id, ty, _, _) in &effects {
        writeln!(s, "    {id}: {ty},")?;
    }
    for name in &colors {
        writeln!(s, "    {}: color,", ident(name, "color."))?;
    }
    for name in &lengths {
        writeln!(s, "    {}: length,", ident(name, ""))?;
    }
    for name in &durations {
        writeln!(s, "    {}: duration,", ident(name, ""))?;
    }
    for name in &styles {
        let id = ident(name, "");
        writeln!(s, "    {id}-size: length,")?;
        writeln!(s, "    {id}-line: length,")?;
        writeln!(s, "    {id}-weight: int,")?;
    }
    writeln!(s, "}}")?;
    writeln!(s, "export global Theme {{")?;
    writeln!(
        s,
        "    // Slot 0 while the OS scheme is dark, slot 1 while it is light; set by the app."
    )?;
    writeln!(s, "    in-out property <[ThemeSlot]> slots: [")?;
    for base in [0, 1] {
        let (_, c) = &t.themes[base];
        write!(
            s,
            "        {{ panel-outline: {}, button-outline: {}, font-family: \"\"",
            panel_outline(c["color.surface.raised"], c["color.surface.base"]),
            BUTTON_FILLS
                .iter()
                .any(|f| BUTTON_SURFACES.iter().any(|n| button_outline(c[*f], c[*n])))
        )?;
        // Built-in themes are calm (ADR-0120 decision 4).
        write!(s, ", halo-clear: 0px")?;
        for (id, _, calm, _) in &effects {
            write!(s, ", {id}: {calm}")?;
        }
        for n in &colors {
            write!(s, ", {}: {}", ident(n, "color."), c[*n])?;
        }
        for n in &lengths {
            write!(s, ", {}: {}px", ident(n, ""), t.length[*n])?;
        }
        for n in &durations {
            write!(s, ", {}: {}ms", ident(n, ""), t.duration[*n])?;
        }
        for n in &styles {
            let id = ident(n, "");
            let st = t.type_styles[*n];
            write!(
                s,
                ", {id}-size: {}px, {id}-line: {}px, {id}-weight: {}",
                st.size, st.line, st.weight
            )?;
        }
        writeln!(s, " }},")?;
    }
    writeln!(s, "    ];")?;
    writeln!(
        s,
        "    // -1 follows Palette. 0 or 1 is the OS setting, used where a menu request\
         \n    // would change what Palette reports (Windows)."
    )?;
    writeln!(s, "    in-out property <int> os-scheme: -1;")?;
    writeln!(
        s,
        "    out property <int> active: os-scheme == 0 || os-scheme == 1 ? os-scheme : Palette.color-scheme == ColorScheme.light ? 1 : 0;"
    )?;
    writeln!(
        s,
        "    // Docked panels and Settings cards draw their outline (ADR-0112 decision 3)."
    )?;
    writeln!(
        s,
        "    out property <bool> panel-outline: slots[active].panel-outline;"
    )?;
    writeln!(s, "    // Secondary buttons draw their outline (ADR-0118).")?;
    writeln!(
        s,
        "    out property <bool> button-outline: slots[active].button-outline;"
    )?;
    writeln!(
        s,
        "    // Theme effects (ADR-0120): drawn only by library components (check-ui)."
    )?;
    for (id, ty, _, _) in &effects {
        writeln!(s, "    out property <{ty}> {id}: slots[active].{id};")?;
    }
    writeln!(
        s,
        "    // The clear zone a position that opts in to the primary's halo keeps round it\
         \n    // (ADR-0122 decision 8): 0 unless the theme draws a halo. A length for layouts,\
         \n    // not an effect."
    )?;
    writeln!(
        s,
        "    out property <length> halo-clear: slots[active].halo-clear;"
    )?;
    writeln!(s, "    // The window font: empty is the OS UI font.")?;
    writeln!(
        s,
        "    out property <string> font-family: slots[active].font-family;"
    )?;
    for (name, c) in &t.fixed {
        writeln!(
            s,
            "    out property <color> {}: {c};",
            ident(name, "color.")
        )?;
    }
    for name in &colors {
        let id = ident(name, "color.");
        writeln!(s, "    out property <color> {id}: slots[active].{id};")?;
    }
    writeln!(s, "}}")?;
    writeln!(s, "export global Tokens {{")?;
    for (name, v) in &t.length {
        let id = ident(name, "");
        if themeable(name) {
            writeln!(
                s,
                "    out property <length> {id}: Theme.slots[Theme.active].{id};"
            )?;
        } else {
            writeln!(s, "    in-out property <length> {id}: {v}px;")?;
        }
    }
    for (name, v) in &t.duration {
        let id = ident(name, "");
        if themeable(name) {
            writeln!(
                s,
                "    out property <duration> {id}: Theme.slots[Theme.active].{id};"
            )?;
        } else {
            writeln!(s, "    in-out property <duration> {id}: {v}ms;")?;
        }
    }
    for name in &styles {
        let id = ident(name, "");
        for (part, ty) in [("size", "length"), ("line", "length"), ("weight", "int")] {
            writeln!(
                s,
                "    out property <{ty}> {id}-{part}: Theme.slots[Theme.active].{id}-{part};"
            )?;
        }
    }
    writeln!(s, "}}")?;
    Ok(s)
}

/// `tokens_apply.rs`: `slot` builds a `ThemeSlot` from a resolved theme, and
/// `apply_tokens` copies the slots and the single values into a window's globals.
/// Included into the app next to the Slint code, so a token added to the file needs no
/// hand-written setter.
pub fn tokens_apply_rs(t: &Tokens) -> Result<String, std::fmt::Error> {
    let mut s = String::new();
    writeln!(
        s,
        "// Generated by rbs-ui tokens_slint from crates/rbs-ui/tokens.toml. Do not edit."
    )?;
    writeln!(
        s,
        "/// The slot for `theme`, with `font` as the window font (empty: the OS UI font).\n\
         /// Fails if `theme` lacks a token the app uses (only for a token file of another\n\
         /// build)."
    )?;
    writeln!(
        s,
        "pub fn slot(theme: &rbs_ui::tokens::ResolvedTheme, font: &str) -> Result<ThemeSlot, rbs_ui::tokens::MissingToken> {{"
    )?;
    writeln!(
        s,
        "    let color = |n: &str| theme.colors.get(n).map(|c| slint::Color::from_argb_u8(c.a, c.r, c.g, c.b)).ok_or_else(|| rbs_ui::tokens::MissingToken(n.to_owned()));"
    )?;
    writeln!(
        s,
        "    let length = |n: &str| theme.length.get(n).copied().ok_or_else(|| rbs_ui::tokens::MissingToken(n.to_owned()));"
    )?;
    writeln!(
        s,
        "    let duration = |n: &str| theme.duration.get(n).map(|v| i64::from(*v)).ok_or_else(|| rbs_ui::tokens::MissingToken(n.to_owned()));"
    )?;
    writeln!(
        s,
        "    let style = |n: &str| theme.type_styles.get(n).copied().ok_or_else(|| rbs_ui::tokens::MissingToken(n.to_owned()));"
    )?;
    for name in t.type_styles.keys() {
        writeln!(s, "    let {} = style(\"{name}\")?;", rust(name, ""))?;
    }
    writeln!(
        s,
        "    let argb = |c: rbs_ui::tokens::Color| slint::Color::from_argb_u8(c.a, c.r, c.g, c.b);"
    )?;
    writeln!(s, "    let fx = theme.effect_draw();")?;
    writeln!(s, "    Ok(ThemeSlot {{")?;
    writeln!(s, "        panel_outline: theme.panel_outline(),")?;
    writeln!(s, "        button_outline: theme.button_outline(),")?;
    writeln!(s, "        font_family: font.into(),")?;
    writeln!(s, "        halo_clear: fx.halo_clear,")?;
    for (id, _, _, expr) in effect_fields() {
        writeln!(s, "        {}: {expr},", id.replace('-', "_"))?;
    }
    for name in t.themes[0].1.keys() {
        writeln!(s, "        {}: color(\"{name}\")?,", rust(name, "color."))?;
    }
    for name in t.length.keys().filter(|n| themeable(n)) {
        writeln!(s, "        {}: length(\"{name}\")?,", rust(name, ""))?;
    }
    for name in t.duration.keys().filter(|n| themeable(n)) {
        writeln!(s, "        {}: duration(\"{name}\")?,", rust(name, ""))?;
    }
    for name in t.type_styles.keys() {
        let r = rust(name, "");
        writeln!(s, "        {r}_size: f32::from({r}.size),")?;
        writeln!(s, "        {r}_line: f32::from({r}.line),")?;
        writeln!(s, "        {r}_weight: i32::from({r}.weight),")?;
    }
    writeln!(s, "    }})")?;
    writeln!(s, "}}")?;
    writeln!(
        s,
        "/// Gives a window the two slots and the token file's single values (window sizes,\n\
         /// keyboard steps). Fails, changing nothing, if `file` lacks one of them."
    )?;
    writeln!(
        s,
        "pub fn apply_tokens(theme: &Theme<'_>, tokens: &Tokens<'_>, file: &rbs_ui::tokens::Tokens, slots: [ThemeSlot; 2]) -> Result<(), rbs_ui::tokens::MissingToken> {{"
    )?;
    writeln!(
        s,
        "    let length = |n: &str| file.length.get(n).copied().ok_or_else(|| rbs_ui::tokens::MissingToken(n.to_owned()));"
    )?;
    if t.duration.keys().any(|n| !themeable(n)) {
        writeln!(
            s,
            "    let duration = |n: &str| file.duration.get(n).map(|v| i64::from(*v)).ok_or_else(|| rbs_ui::tokens::MissingToken(n.to_owned()));"
        )?;
    }
    for name in t.length.keys().filter(|n| !themeable(n)) {
        writeln!(s, "    let {} = length(\"{name}\")?;", rust(name, ""))?;
    }
    for name in t.duration.keys().filter(|n| !themeable(n)) {
        writeln!(s, "    let {} = duration(\"{name}\")?;", rust(name, ""))?;
    }
    writeln!(
        s,
        "    theme.set_slots(slint::ModelRc::new(slint::VecModel::from(slots.to_vec())));"
    )?;
    for name in t.length.keys().filter(|n| !themeable(n)) {
        let r = rust(name, "");
        writeln!(s, "    tokens.set_{r}({r});")?;
    }
    for name in t.duration.keys().filter(|n| !themeable(n)) {
        let r = rust(name, "");
        writeln!(s, "    tokens.set_{r}({r});")?;
    }
    writeln!(s, "    Ok(())")?;
    writeln!(s, "}}")?;
    writeln!(
        s,
        "/// Names of the tokens a slot holds, for the golden test (`look`)."
    )?;
    write!(s, "pub const SLOT_TOKENS: &[&str] = &[")?;
    for name in t.themes[0]
        .1
        .keys()
        .chain(t.length.keys().filter(|n| themeable(n)))
        .chain(t.duration.keys().filter(|n| themeable(n)))
        .chain(t.type_styles.keys())
    {
        write!(s, "\"{name}\", ")?;
    }
    writeln!(s, "];")?;
    writeln!(
        s,
        "/// The effect fields of a slot (ADR-0120, ADR-0122), for the golden test (`look`)."
    )?;
    write!(s, "pub const SLOT_EFFECTS: &[&str] = &[")?;
    for (id, _, _, _) in effect_fields() {
        write!(s, "\"{id}\", ")?;
    }
    writeln!(s, "];")?;
    Ok(s)
}

/// Writes a generated file and gives it `modified` (the token file's modification time).
/// slint-build asks cargo to rerun a UI build script when a file the UI includes changes,
/// and `tokens.slint` is one of them: a file written during the run is newer than the run
/// itself, so without the older time every cargo command saw the UI as changed and
/// rebuilt it (about a minute per CI step). Backdating is safe: whenever the content can
/// change, `tokens.toml` changed, the script runs again and the UI is rebuilt anyway.
pub fn write_generated(path: &Path, text: &str, modified: SystemTime) -> std::io::Result<()> {
    std::fs::write(path, text)?;
    std::fs::File::options()
        .write(true)
        .open(path)?
        .set_modified(modified)
}

/// Why [`write_token_files`] failed; the text names the file.
#[derive(Debug)]
pub enum TokenFilesError {
    /// The built-in token file does not load.
    Tokens(TokenError),
    /// Generating a file's text failed.
    Generate(&'static str, fmt::Error),
    /// Writing a file failed.
    Write(&'static str, std::io::Error),
}

impl fmt::Display for TokenFilesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenFilesError::Tokens(e) => e.fmt(f),
            TokenFilesError::Generate(file, e) => write!(f, "generating {file}: {e}"),
            TokenFilesError::Write(file, e) => write!(f, "writing {file}: {e}"),
        }
    }
}

impl std::error::Error for TokenFilesError {}

/// Writes `tokens.slint` and `tokens_apply.rs` for the built-in token file into `dir`,
/// with `modified` as their modification time (see [`write_generated`]).
pub fn write_token_files(dir: &Path, modified: SystemTime) -> Result<(), TokenFilesError> {
    let tokens = Tokens::built_in().map_err(TokenFilesError::Tokens)?;
    for (file, text) in [
        ("tokens.slint", tokens_slint(&tokens)),
        ("tokens_apply.rs", tokens_apply_rs(&tokens)),
    ] {
        let text = text.map_err(|e| TokenFilesError::Generate(file, e))?;
        write_generated(&dir.join(file), &text, modified)
            .map_err(|e| TokenFilesError::Write(file, e))?;
    }
    Ok(())
}
