//! ITU-T H.273 code points for the colour description of a video bitstream.
//!
//! H.264 and HEVC VUI, AV1 colour config and most hardware encoder APIs (NVENC, AMF,
//! oneVPL, VA-API) take the same numbers. `None` means `rbs_core` has a value H.273
//! cannot express for coded Y'CbCr video (for example the `Rgb` matrix), and the
//! encoder should refuse the configuration instead of signalling something wrong.

use rbs_core::media::{
    ColorPrimaries, ColorRange, ColorSpace, MatrixCoefficients, TransferFunction,
};

/// The H.273 code points of one colour description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorCodes {
    /// `colour_primaries`.
    pub primaries: u8,
    /// `transfer_characteristics`.
    pub transfer: u8,
    /// `matrix_coefficients`.
    pub matrix: u8,
    /// `video_full_range_flag`.
    pub full_range: bool,
}

/// `colour_primaries` of `primaries`.
pub const fn primaries(primaries: ColorPrimaries) -> Option<u8> {
    match primaries {
        ColorPrimaries::Bt709 => Some(1),
        ColorPrimaries::Bt601Pal => Some(5),
        ColorPrimaries::Bt601Ntsc => Some(6),
        ColorPrimaries::Bt2020 => Some(9),
        // SMPTE EG 432-1 (P3 with D65 white).
        ColorPrimaries::DisplayP3 => Some(12),
        #[allow(unreachable_patterns)] // `ColorPrimaries` is non-exhaustive.
        _ => None,
    }
}

/// `transfer_characteristics` of `transfer`.
pub const fn transfer(transfer: TransferFunction) -> Option<u8> {
    match transfer {
        TransferFunction::Bt709 => Some(1),
        TransferFunction::Linear => Some(8),
        TransferFunction::Srgb => Some(13),
        TransferFunction::Pq => Some(16),
        TransferFunction::Hlg => Some(18),
        #[allow(unreachable_patterns)] // `TransferFunction` is non-exhaustive.
        _ => None,
    }
}

/// `matrix_coefficients` of `matrix` for Y'CbCr video. BT.601 is signalled as
/// SMPTE 170M (6) unless the primaries are the 625-line ones, then as BT.470BG (5);
/// both name the same matrix. `Rgb` is `None`: 4:2:0 video is never RGB.
pub const fn matrix(matrix: MatrixCoefficients, primaries: ColorPrimaries) -> Option<u8> {
    match matrix {
        MatrixCoefficients::Bt709 => Some(1),
        MatrixCoefficients::Bt601 => Some(if matches!(primaries, ColorPrimaries::Bt601Pal) {
            5
        } else {
            6
        }),
        MatrixCoefficients::Bt2020Ncl => Some(9),
        _ => None,
    }
}

impl ColorCodes {
    /// The code points of `color`, or `None` if one of its values has none.
    pub const fn of(color: &ColorSpace) -> Option<Self> {
        let (Some(primaries), Some(transfer), Some(matrix)) = (
            primaries(color.primaries),
            transfer(color.transfer),
            matrix(color.matrix, color.primaries),
        ) else {
            return None;
        };
        Some(Self {
            primaries,
            transfer,
            matrix,
            full_range: matches!(color.range, ColorRange::Full),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt709_limited() {
        assert_eq!(
            ColorCodes::of(&ColorSpace::BT709_LIMITED),
            Some(ColorCodes {
                primaries: 1,
                transfer: 1,
                matrix: 1,
                full_range: false,
            })
        );
    }

    #[test]
    fn rgb_is_not_video() {
        assert_eq!(ColorCodes::of(&ColorSpace::SRGB), None);
    }

    #[test]
    fn srgb_video_is_full_range_iec61966() {
        assert_eq!(
            ColorCodes::of(&ColorSpace::SRGB_VIDEO),
            Some(ColorCodes {
                primaries: 1,
                transfer: 13,
                matrix: 1,
                full_range: true,
            })
        );
    }

    #[test]
    fn hdr_and_601() {
        let pq = ColorSpace::new(
            ColorPrimaries::Bt2020,
            TransferFunction::Pq,
            MatrixCoefficients::Bt2020Ncl,
            ColorRange::Limited,
        );
        assert_eq!(
            ColorCodes::of(&pq).map(|c| (c.primaries, c.transfer, c.matrix)),
            Some((9, 16, 9))
        );
        assert_eq!(
            matrix(MatrixCoefficients::Bt601, ColorPrimaries::Bt601Pal),
            Some(5)
        );
        assert_eq!(
            matrix(MatrixCoefficients::Bt601, ColorPrimaries::Bt601Ntsc),
            Some(6)
        );
        assert_eq!(transfer(TransferFunction::Hlg), Some(18));
        assert_eq!(primaries(ColorPrimaries::DisplayP3), Some(12));
    }
}
