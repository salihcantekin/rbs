//! Small CoreFoundation and VideoToolbox helpers. Set-up code only; nothing here runs
//! per frame.

use objc2_core_foundation::{
    CFArray, CFBoolean, CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::CMFormatDescription;
use objc2_core_media::kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms;
use objc2_video_toolbox::{VTCompressionSession, VTSessionSetProperty};

/// A VideoToolbox status code (`OSStatus`).
pub(crate) type Status = i32;

/// Sets one session property.
pub(crate) fn set_property(
    session: &VTCompressionSession,
    key: &CFString,
    value: &CFType,
) -> Result<(), Status> {
    // SAFETY: a compression session is a VTSession (a CFType); key and value are valid
    // CoreFoundation objects for the duration of the call, which retains what it keeps.
    let status = unsafe { VTSessionSetProperty(session, key, Some(value)) };
    if status == 0 { Ok(()) } else { Err(status) }
}

/// Sets a boolean property.
pub(crate) fn set_bool(
    session: &VTCompressionSession,
    key: &CFString,
    value: bool,
) -> Result<(), Status> {
    set_property(session, key, CFBoolean::new(value))
}

/// Sets an integer property.
pub(crate) fn set_i64(
    session: &VTCompressionSession,
    key: &CFString,
    value: i64,
) -> Result<(), Status> {
    set_property(session, key, &CFNumber::new_i64(value))
}

/// Sets a floating-point property.
pub(crate) fn set_f64(
    session: &VTCompressionSession,
    key: &CFString,
    value: f64,
) -> Result<(), Status> {
    set_property(session, key, &CFNumber::new_f64(value))
}

/// `kVTCompressionPropertyKey_DataRateLimits`: at most `bytes` per `seconds`.
pub(crate) fn data_rate_limits(bytes: i64, seconds: f64) -> CFRetained<CFArray<CFNumber>> {
    let values = [CFNumber::new_i64(bytes), CFNumber::new_f64(seconds)];
    CFArray::from_retained_objects(&values)
}

/// A dictionary with CFString keys.
pub(crate) fn dictionary(
    keys: &[&CFString],
    values: &[&CFType],
) -> CFRetained<CFDictionary<CFString, CFType>> {
    CFDictionary::from_slices(keys, values)
}

/// The CFData of the `atom` record (`avcC` or `hvcC`) of a video format description.
/// Retains only; never allocates.
fn decoder_config_data(desc: &CMFormatDescription, atom: &CFString) -> Option<CFRetained<CFData>> {
    // SAFETY: the key is a CoreFoundation constant that lives for the process.
    let key = unsafe { kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms };
    // SAFETY: `desc` is a valid format description; the getter returns a retained value.
    let atoms = unsafe { desc.extension(key) }?;
    let atoms = atoms.downcast::<CFDictionary>().ok()?;
    // SAFETY: the sample description atoms are a dictionary from atom names (CFString)
    // to their payloads (CFData, or an array of CFData for repeated atoms).
    let atoms: &CFDictionary<CFString, CFType> = unsafe { atoms.cast_unchecked() };
    atoms.get(atom)?.downcast::<CFData>().ok()
}

/// The `avcC` or `hvcC` record of a video format description, copied.
pub(crate) fn decoder_config_record(
    desc: &CMFormatDescription,
    atom: &CFString,
) -> Option<Vec<u8>> {
    decoder_config_data(desc, atom).map(|d| d.to_vec())
}

/// True when the `atom` record of `desc` equals `record`. Called for keyframes from the
/// output callback: compares in place, without allocating.
pub(crate) fn decoder_config_matches(
    desc: &CMFormatDescription,
    atom: &CFString,
    record: &[u8],
) -> bool {
    let Some(data) = decoder_config_data(desc, atom) else {
        return false;
    };
    // SAFETY: the bytes belong to `data`, an immutable CFData retained for this scope.
    unsafe { data.as_bytes_unchecked() == record }
}
