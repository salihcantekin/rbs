//! The built-in filter types (spec `video-filters.md`, "Catalogue").

use rbs_graph::node::NodeTypeId;

use crate::types::{Field, FieldKind, FilterType, Pass, PassSource, Scale, Uniforms, Values};

/// Type id of colour correction.
pub const COLOR_CORRECTION: &str = "org.rbs.filter.color-correction";
/// Type id of chroma key.
pub const CHROMA_KEY: &str = "org.rbs.filter.chroma-key";
/// Type id of blur.
pub const BLUR: &str = "org.rbs.filter.blur";
/// Type id of sharpen.
pub const SHARPEN: &str = "org.rbs.filter.sharpen";

/// Every built-in filter type, in the order the UI lists them.
pub fn catalogue() -> &'static [&'static dyn FilterType] {
    &[&ColorCorrection, &ChromaKey, &Blur, &Sharpen]
}

/// The built-in filter type with this id.
pub fn find(type_id: &str) -> Option<&'static dyn FilterType> {
    catalogue()
        .iter()
        .copied()
        .find(|t| t.type_id().as_str() == type_id)
}

const fn number(
    key: &'static str,
    label: &'static str,
    min: f32,
    max: f32,
    default: f32,
    step: f32,
    unit: &'static str,
) -> Field {
    Field {
        key,
        label,
        kind: FieldKind::Number {
            min,
            max,
            default,
            step,
            unit,
        },
    }
}

/// sRGB-encoded value of an 8-bit channel.
fn unit(c: u8) -> f32 {
    f32::from(c) / 255.0
}

/// BT.709 chroma (Cb, Cr) of sRGB-encoded `rgb`, as the shader computes it.
fn chroma(rgb: [f32; 3]) -> [f32; 2] {
    let y = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
    [(rgb[2] - y) / 1.8556, (rgb[0] - y) / 1.5748]
}

/// Colour correction: brightness, contrast, saturation, gamma and hue.
#[derive(Debug, Clone, Copy, Default)]
pub struct ColorCorrection;

const COLOR_FIELDS: [Field; 5] = [
    number("brightness", "Brightness", -1.0, 1.0, 0.0, 0.01, ""),
    number("contrast", "Contrast", -1.0, 1.0, 0.0, 0.01, ""),
    number("saturation", "Saturation", 0.0, 3.0, 1.0, 0.01, ""),
    number("gamma", "Gamma", 0.1, 4.0, 1.0, 0.01, ""),
    number("hue", "Hue", -180.0, 180.0, 0.0, 1.0, "°"),
];
const COLOR_PASSES: [Pass; 1] = [Pass {
    shader: include_str!("shaders/color_correction.wgsl"),
    target: Scale::Full,
    source: PassSource::Input,
}];

impl FilterType for ColorCorrection {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new_static(COLOR_CORRECTION)
    }
    fn name(&self) -> &'static str {
        "Colour correction"
    }
    fn fields(&self) -> &'static [Field] {
        &COLOR_FIELDS
    }
    fn passes(&self) -> &'static [Pass] {
        &COLOR_PASSES
    }
    fn uniforms(&self, values: &Values, _input: [u32; 2], out: &mut [Uniforms]) {
        let Some(u) = out.first_mut() else { return };
        let gamma = values.number(3).max(0.1);
        let hue = values.number(4).to_radians();
        u.0[0] = [
            values.number(0),
            values.number(1),
            values.number(2),
            1.0 / gamma,
        ];
        u.0[1] = [hue.cos(), hue.sin(), 0.0, 0.0];
    }
}

/// Chroma key: makes a colour (green by default) transparent.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChromaKey;

const KEY_FIELDS: [Field; 4] = [
    Field {
        key: "key_color",
        label: "Key colour",
        kind: FieldKind::Color {
            default: [0, 255, 0, 255],
        },
    },
    number("similarity", "Similarity", 0.0, 1.0, 0.4, 0.01, ""),
    number("smoothness", "Smoothness", 0.0, 1.0, 0.08, 0.01, ""),
    number("spill", "Spill reduction", 0.0, 1.0, 0.1, 0.01, ""),
];
const KEY_PASSES: [Pass; 1] = [Pass {
    shader: include_str!("shaders/chroma_key.wgsl"),
    target: Scale::Full,
    source: PassSource::Input,
}];

impl FilterType for ChromaKey {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new_static(CHROMA_KEY)
    }
    fn name(&self) -> &'static str {
        "Chroma key"
    }
    fn fields(&self) -> &'static [Field] {
        &KEY_FIELDS
    }
    fn passes(&self) -> &'static [Pass] {
        &KEY_PASSES
    }
    fn uniforms(&self, values: &Values, _input: [u32; 2], out: &mut [Uniforms]) {
        let Some(u) = out.first_mut() else { return };
        let k = values.color(0);
        let [cb, cr] = chroma([unit(k[0]), unit(k[1]), unit(k[2])]);
        let len = (cb * cb + cr * cr).sqrt();
        let dir = if len > 1e-6 {
            [cb / len, cr / len]
        } else {
            [0.0, 0.0]
        };
        u.0[0] = [cb, cr, values.number(1), values.number(2)];
        u.0[1] = [values.number(3), dir[0], dir[1], 0.0];
    }
}

/// Blur: a separable Gaussian, horizontal at half size, vertical back to full size.
#[derive(Debug, Clone, Copy, Default)]
pub struct Blur;

/// Taps on each side of the centre, at most.
pub const MAX_BLUR_TAPS: usize = 31;

const BLUR_FIELDS: [Field; 1] = [number("radius", "Radius", 0.0, 64.0, 8.0, 1.0, "px")];
const BLUR_PASSES: [Pass; 2] = [
    Pass {
        shader: include_str!("shaders/blur.wgsl"),
        target: Scale::Half,
        source: PassSource::Input,
    },
    Pass {
        shader: include_str!("shaders/blur.wgsl"),
        target: Scale::Full,
        source: PassSource::Previous,
    },
];

/// Normalised one-sided Gaussian weights for `sigma` (in half-size texels) and the
/// number of taps on each side.
fn gaussian(sigma: f32, weights: &mut [f32; MAX_BLUR_TAPS + 1]) -> usize {
    *weights = [0.0; MAX_BLUR_TAPS + 1];
    if sigma < 0.25 {
        weights[0] = 1.0;
        return 0;
    }
    let n = ((3.0 * sigma).ceil() as usize).min(MAX_BLUR_TAPS);
    let mut sum = 0.0;
    for (k, w) in weights.iter_mut().enumerate().take(n + 1) {
        let x = k as f32;
        *w = (-(x * x) / (2.0 * sigma * sigma)).exp();
        sum += if k == 0 { *w } else { 2.0 * *w };
    }
    for w in weights.iter_mut().take(n + 1) {
        *w /= sum;
    }
    n
}

impl FilterType for Blur {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new_static(BLUR)
    }
    fn name(&self) -> &'static str {
        "Blur"
    }
    fn fields(&self) -> &'static [Field] {
        &BLUR_FIELDS
    }
    fn passes(&self) -> &'static [Pass] {
        &BLUR_PASSES
    }
    fn uniforms(&self, values: &Values, input: [u32; 2], out: &mut [Uniforms]) {
        // sigma = radius / 2 source pixels, in half-size texels.
        let sigma = values.number(0) / 4.0;
        let mut weights = [0.0; MAX_BLUR_TAPS + 1];
        let n = gaussian(sigma, &mut weights);
        let half = Scale::Half.of(input);
        let steps = [[1.0 / half[0] as f32, 0.0], [0.0, 1.0 / half[1] as f32]];
        for (u, step) in out.iter_mut().zip(steps) {
            for (i, chunk) in weights.chunks(4).enumerate() {
                for (j, w) in chunk.iter().enumerate() {
                    u.0[i][j] = *w;
                }
            }
            u.0[8] = [n as f32, step[0], step[1], 0.0];
        }
    }
}

/// Sharpen: an unsharp mask on the four neighbours.
#[derive(Debug, Clone, Copy, Default)]
pub struct Sharpen;

const SHARPEN_FIELDS: [Field; 1] = [number("amount", "Amount", 0.0, 2.0, 0.3, 0.01, "")];
const SHARPEN_PASSES: [Pass; 1] = [Pass {
    shader: include_str!("shaders/sharpen.wgsl"),
    target: Scale::Full,
    source: PassSource::Input,
}];

impl FilterType for Sharpen {
    fn type_id(&self) -> NodeTypeId {
        NodeTypeId::new_static(SHARPEN)
    }
    fn name(&self) -> &'static str {
        "Sharpen"
    }
    fn fields(&self) -> &'static [Field] {
        &SHARPEN_FIELDS
    }
    fn passes(&self) -> &'static [Pass] {
        &SHARPEN_PASSES
    }
    fn uniforms(&self, values: &Values, _input: [u32; 2], out: &mut [Uniforms]) {
        if let Some(u) = out.first_mut() {
            u.0[0] = [values.number(0), 0.0, 0.0, 0.0];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gaussian_weights_sum_to_one() {
        let mut w = [0.0; MAX_BLUR_TAPS + 1];
        for sigma in [0.0, 0.3, 1.0, 4.0, 16.0] {
            let n = gaussian(sigma, &mut w);
            let sum: f32 = w[0] + 2.0 * w[1..=n].iter().sum::<f32>();
            assert!((sum - 1.0).abs() < 1e-5, "{sigma}: {sum}");
            assert!(n <= MAX_BLUR_TAPS);
        }
    }

    #[test]
    fn catalogue_ids_are_unique_and_found() {
        for t in catalogue() {
            assert_eq!(find(t.type_id().as_str()).map(|f| f.name()), Some(t.name()));
        }
        assert!(find("org.rbs.filter.none").is_none());
    }
}
