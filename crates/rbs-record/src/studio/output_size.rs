//! The encoded picture size of an output (ADR-0102 decision 7).

use crate::recording::RecordError;

/// Smallest side of an explicit output size (ADR-0102 decision 7).
const OUTPUT_SIZE_MIN: u32 = 2;
/// Largest side of an explicit output size. A canvas may be taller (8192); an
/// absent size keeps that canvas.
const OUTPUT_SIZE_MAX: u32 = 7680;

/// The picture an output encodes. `None` is `canvas`. An explicit size must be even
/// and in 2..=7680 on both sides; otherwise [`RecordError::OutputSize`] and nothing
/// is chosen. The aspect is not re-fitted: a later settings UI stores same-aspect
/// sizes, and a different aspect is letterboxed by the scaled download.
pub(crate) fn resolve_output_size(
    size: Option<(u32, u32)>,
    canvas: (u32, u32),
) -> Result<(u32, u32), RecordError> {
    let Some((width, height)) = size else {
        return Ok(canvas);
    };
    let even = width.is_multiple_of(2) && height.is_multiple_of(2);
    let in_range = (OUTPUT_SIZE_MIN..=OUTPUT_SIZE_MAX).contains(&width)
        && (OUTPUT_SIZE_MIN..=OUTPUT_SIZE_MAX).contains(&height);
    if even && in_range {
        Ok((width, height))
    } else {
        Err(RecordError::OutputSize { width, height })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_size_keeps_the_canvas_and_an_illegal_size_is_refused() {
        assert_eq!(resolve_output_size(None, (64, 36)).unwrap(), (64, 36));
        // A vertical canvas may be 8192. "Same as canvas" is allowed; an explicit
        // side above 7680 is not.
        assert_eq!(
            resolve_output_size(None, (1080, 8192)).unwrap(),
            (1080, 8192)
        );
        assert!(matches!(
            resolve_output_size(Some((1080, 8192)), (1080, 8192)),
            Err(RecordError::OutputSize {
                width: 1080,
                height: 8192
            })
        ));
        assert_eq!(
            resolve_output_size(Some((7680, 2)), (64, 36)).unwrap(),
            (7680, 2)
        );
        assert_eq!(resolve_output_size(Some((2, 2)), (64, 36)).unwrap(), (2, 2));
        // A different aspect is not re-fitted here. The download letterboxes it.
        assert_eq!(
            resolve_output_size(Some((64, 64)), (64, 36)).unwrap(),
            (64, 64)
        );
        for (width, height) in [(33, 18), (1, 2), (0, 36), (7690, 2), (4, 7682), (3, 3)] {
            assert!(
                matches!(
                    resolve_output_size(Some((width, height)), (64, 36)),
                    Err(RecordError::OutputSize { width: w, height: h }) if w == width && h == height
                ),
                "{width}x{height}"
            );
        }
    }
}
