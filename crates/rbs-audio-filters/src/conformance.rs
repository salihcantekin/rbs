//! Conformance checks every audio filter type must pass (ADR-0082, Tests).

use rbs_core::media::AudioFormat;
use rbs_params::{FieldKind, Value, Values};

use crate::{AudioFilterType, DspSetup, MAX_FIELDS};

/// Block size the checks use.
const FRAMES: usize = 480;

/// Checks a type's schema and its DSP on silence, full scale, NaN and infinity, at the
/// default values and at every number field's extremes. Returns every problem.
pub fn check_type(ty: &dyn AudioFilterType) -> Vec<String> {
    let mut problems = Vec::new();
    let id = ty.type_id();
    if !id.is_well_formed() {
        problems.push(format!("type id {id} is not well-formed"));
    }
    if let Err(e) = rbs_params::check_fields(ty.fields(), MAX_FIELDS) {
        problems.push(e);
        return problems;
    }
    let setup = DspSetup::new(AudioFormat::STEREO_48K, FRAMES as u32);
    let mut cases = vec![Values::defaults(ty)];
    for (i, f) in ty.fields().iter().enumerate() {
        if let FieldKind::Number { min, max, .. } = f.kind {
            for v in [min, max] {
                match cases[0].with(ty, [(f.key, Value::Number(v))]) {
                    Ok(values) => cases.push(values),
                    Err(e) => problems.push(format!("{}: {v} refused: {e}", f.key)),
                }
            }
        }
        let _ = i;
    }
    for values in cases {
        let mut dsp = match ty.make(setup, &values) {
            Ok(d) => d,
            Err(e) => {
                problems.push(format!("make refused valid values: {e}"));
                continue;
            }
        };
        dsp.set(&values);
        dsp.reset();
        let inputs: [(&str, f32); 5] = [
            ("silence", 0.0),
            ("full scale", 1.0),
            ("NaN", f32::NAN),
            ("infinity", f32::INFINITY),
            ("denormal", 1.0e-40),
        ];
        for (name, x) in inputs {
            let mut l = vec![x; FRAMES];
            let mut r = vec![-x; FRAMES];
            for _ in 0..4 {
                dsp.process(&mut [&mut l[..], &mut r[..]], FRAMES);
            }
            let bad = |s: &[f32]| {
                s.iter()
                    .any(|y| !y.is_finite() || (*y != 0.0 && y.abs() < f32::MIN_POSITIVE))
            };
            if bad(&l) || bad(&r) {
                problems.push(format!("{name}: output has NaN, infinity or a denormal"));
            }
        }
    }
    problems
}
