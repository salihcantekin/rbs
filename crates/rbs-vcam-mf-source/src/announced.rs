//! The one format the camera announces (ADR-0066 decision 7), checked against the
//! source's own limits. Values from outside (the user's format file) that break them fall back to
//! the default, so a bad value never reaches Media Foundation.

/// Sizes the source announces (ADR-0066 decision 11).
pub const SIZES: [(u32, u32); 3] = [(1920, 1080), (1280, 720), (640, 360)];
/// Frame rates the source announces (frames per second, denominator 1).
pub const RATES: [u32; 4] = [15, 24, 30, 60];
/// Layout version of the format file (ADR-0070 decision 3).
pub const FORMAT_VERSION: u32 = 1;
/// Bytes of the format file: version, width, height, rate (little-endian).
pub const FORMAT_LEN: usize = 16;

/// The format file of `sid`, relative to `%ProgramData%` (ADR-0070 decision 3).
pub fn format_file(sid: &str) -> String {
    format!("RBS\\VirtualCamera\\{sid}.fmt")
}

/// An NV12 format the source can announce: one of [`SIZES`] at one of [`RATES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Announced {
    width: u32,
    height: u32,
    rate: u32,
}

impl Default for Announced {
    /// 1280 x 720 at 30 fps.
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            rate: 30,
        }
    }
}

impl Announced {
    /// The format, if it is within the limits.
    pub fn new(width: u32, height: u32, rate_num: u32, rate_den: u32) -> Option<Self> {
        let ok = SIZES.contains(&(width, height)) && rate_den == 1 && RATES.contains(&rate_num);
        ok.then_some(Self {
            width,
            height,
            rate: rate_num,
        })
    }

    /// The format, or the default when any value breaks the limits.
    pub fn or_default(width: u32, height: u32, rate_num: u32, rate_den: u32) -> Self {
        Self::new(width, height, rate_num, rate_den).unwrap_or_default()
    }

    /// The format a format file holds (ADR-0070 decision 3), or the default
    /// for anything else: another length or version, or a size or rate not offered.
    pub fn from_value(bytes: &[u8]) -> Self {
        let Ok(b) = <[u8; FORMAT_LEN]>::try_from(bytes) else {
            return Self::default();
        };
        let word = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        if word(0) != FORMAT_VERSION {
            return Self::default();
        }
        Self::or_default(word(4), word(8), word(12), 1)
    }

    /// The format file's content for this format.
    pub fn to_value(&self) -> [u8; FORMAT_LEN] {
        let mut b = [0u8; FORMAT_LEN];
        for (i, v) in [FORMAT_VERSION, self.width, self.height, self.rate]
            .into_iter()
            .enumerate()
        {
            b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Frames per second.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Bytes of one tightly packed NV12 frame (stride = width).
    pub fn frame_len(&self) -> usize {
        let (w, h) = (self.width as usize, self.height as usize);
        w * h + w * (h / 2)
    }

    /// Duration of one frame in 100 ns units (Media Foundation's time unit).
    pub fn frame_duration_hns(&self) -> i64 {
        10_000_000 / i64::from(self.rate)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;

    #[test]
    fn values_outside_the_limits_fall_back_to_the_default() {
        assert_eq!(
            Announced::or_default(1920, 1080, 60, 1),
            Announced::new(1920, 1080, 60, 1).unwrap_or_default()
        );
        assert_eq!(Announced::or_default(1920, 1080, 60, 1).width(), 1920);
        for (w, h, n, d) in [
            (0, 720, 30, 1),
            (2, 2, 30, 1),
            (1920, 720, 30, 1),
            (1281, 720, 30, 1),
            (3840, 2160, 30, 1),
            (1280, 720, 25, 1),
            (1280, 720, 30000, 1001),
            (1280, 720, 30, 0),
        ] {
            assert_eq!(Announced::or_default(w, h, n, d), Announced::default());
        }
    }

    #[test]
    fn the_format_file_round_trips_and_bad_values_give_the_default() {
        let f = Announced::new(640, 360, 60, 1).unwrap();
        assert_eq!(Announced::from_value(&f.to_value()), f);
        assert_eq!(Announced::from_value(&[]), Announced::default());
        assert_eq!(
            Announced::from_value(&f.to_value()[..15]),
            Announced::default()
        );
        let mut v = f.to_value();
        v[0] = 2; // another version
        assert_eq!(Announced::from_value(&v), Announced::default());
        let mut v = f.to_value();
        v[4] = 0x81; // width 641
        v[5] = 0x02;
        assert_eq!(Announced::from_value(&v), Announced::default());
    }

    #[test]
    fn sizes_and_durations() {
        let f = Announced::default();
        assert_eq!(f.frame_len(), 1280 * 720 * 3 / 2);
        assert_eq!(f.frame_duration_hns(), 333_333);
    }
}
