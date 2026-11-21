//! The `serde` defaults and skip tests of the wire types in [`super`].

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
pub(super) fn is_false(v: &bool) -> bool {
    !*v
}

pub(super) fn default_pcm_bits() -> u16 {
    16
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
pub(super) fn is_pcm_16(bits: &u16) -> bool {
    *bits == 16
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
pub(super) fn is_zero_ms(v: &i32) -> bool {
    *v == 0
}

pub(super) fn is_zero_f64(v: &f64) -> bool {
    *v == 0.0
}

pub(super) fn shown() -> bool {
    true
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
pub(super) fn is_shown(v: &bool) -> bool {
    *v
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference.
pub(super) fn is_unlocked(v: &bool) -> bool {
    !*v
}

pub(super) fn full_volume() -> f32 {
    1.0
}

pub(super) fn default_ramp_ms() -> u32 {
    300
}

pub(super) fn is_default_ramp_ms(v: &u32) -> bool {
    *v == 300
}

pub(super) fn is_zero_f32(v: &f32) -> bool {
    *v == 0.0
}

pub(super) fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}
