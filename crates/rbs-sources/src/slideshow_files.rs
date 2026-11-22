//! The pictures of a slide show source (ADR-0171 decisions 2, 5 and 6): listing a folder
//! in natural name order, the watch stamp, the shuffle order and fitting a picture into
//! the output size. Pure functions for the decode thread and the control side; nothing
//! here runs in a node's `process`.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::still::Picture;

/// Most pictures a slide show lists; the rest are left out with a warning.
pub const MAX_FILES: usize = 1000;

/// One listed picture file and what the watch check compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The file name (no folder).
    pub name: String,
    /// Its size in bytes.
    pub size: u64,
    /// Its modification time, when the file system gives one.
    pub modified: Option<SystemTime>,
}

/// A folder's pictures in show order before any shuffle.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Listing {
    /// At most [`MAX_FILES`] entries, in natural name order.
    pub entries: Vec<Entry>,
    /// How many matching files past [`MAX_FILES`] were left out.
    pub left_out: usize,
}

impl Listing {
    /// The full path of entry `index`.
    pub fn path(&self, folder: &Path, index: usize) -> Option<PathBuf> {
        self.entries.get(index).map(|e| folder.join(&e.name))
    }
}

/// True for a name the slide show shows: not hidden, extension `png`, `jpg` or `jpeg`
/// in any case.
pub fn is_picture_name(name: &str) -> bool {
    if name.starts_with('.') {
        return false;
    }
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    ["png", "jpg", "jpeg"]
        .iter()
        .any(|e| ext.eq_ignore_ascii_case(e))
}

/// Lists the pictures directly in `folder`: regular files only (no symbolic links, no
/// subfolders), hidden names skipped, natural order, capped after sorting.
pub fn list_folder(folder: &Path) -> Result<Listing, String> {
    let read = std::fs::read_dir(folder).map_err(|e| e.to_string())?;
    let mut entries = Vec::new();
    for item in read {
        let Ok(item) = item else { continue };
        // `file_type` does not follow symbolic links, so a link is never a file here.
        let Ok(kind) = item.file_type() else { continue };
        if !kind.is_file() {
            continue;
        }
        let Ok(name) = item.file_name().into_string() else {
            continue;
        };
        if !is_picture_name(&name) {
            continue;
        }
        let meta = item.metadata().ok();
        entries.push(Entry {
            name,
            size: meta.as_ref().map_or(0, std::fs::Metadata::len),
            modified: meta.and_then(|m| m.modified().ok()),
        });
    }
    Ok(sorted_listing(entries))
}

/// Sorts entries in natural order and applies the cap.
pub fn sorted_listing(mut entries: Vec<Entry>) -> Listing {
    entries.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    let left_out = entries.len().saturating_sub(MAX_FILES);
    entries.truncate(MAX_FILES);
    Listing { entries, left_out }
}

/// Natural name order: runs of ASCII digits compare by value (`2` before `10`), the
/// rest case-insensitively; ties are broken by the exact names, so the order is total.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    natural_key_cmp(a, b).then_with(|| a.cmp(b))
}

fn natural_key_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.as_bytes(), b.as_bytes());
    loop {
        match (a.first(), b.first()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let da = a.iter().take_while(|c| c.is_ascii_digit()).count();
                let db = b.iter().take_while(|c| c.is_ascii_digit()).count();
                let (na, nb) = (trim_zeros(&a[..da]), trim_zeros(&b[..db]));
                // Longer without leading zeros is larger; same length compares digit by digit.
                let order = na.len().cmp(&nb.len()).then_with(|| na.cmp(nb));
                if order != Ordering::Equal {
                    return order;
                }
                a = &a[da..];
                b = &b[db..];
            }
            (Some(x), Some(y)) => {
                let order = x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase());
                if order != Ordering::Equal {
                    return order;
                }
                a = &a[1..];
                b = &b[1..];
            }
        }
    }
}

fn trim_zeros(digits: &[u8]) -> &[u8] {
    let zeros = digits.iter().take_while(|&&c| c == b'0').count();
    &digits[zeros.min(digits.len().saturating_sub(1))..]
}

/// A small deterministic generator for the shuffle (xorshift64*). The decode thread seeds
/// it once per source; tests give a fixed seed.
#[derive(Debug, Clone)]
pub struct Shuffler {
    state: u64,
}

impl Shuffler {
    /// A generator from `seed` (0 is replaced, since xorshift would stay at 0).
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    fn next(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A new pass over `count` pictures (Fisher-Yates). When `last` is the previous
    /// pass's final picture and the new pass would start with it, the first two swap,
    /// so the same picture never shows twice in a row.
    pub fn pass(&mut self, count: usize, last: Option<usize>) -> Vec<usize> {
        let mut order: Vec<usize> = (0..count).collect();
        for i in (1..count).rev() {
            let j = (self.next() % (i as u64 + 1)) as usize;
            order.swap(i, j);
        }
        if count > 1 && last.is_some() && order.first().copied() == last {
            order.swap(0, 1);
        }
        order
    }
}

/// How a picture fills the output (setting `fit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    /// The whole picture, centred, with transparent bars.
    #[default]
    Contain,
    /// The output filled, the picture's centre kept, the rest cropped.
    Cover,
    /// The output filled, the aspect ratio ignored.
    Stretch,
}

impl Fit {
    /// The setting value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Contain => "contain",
            Self::Cover => "cover",
            Self::Stretch => "stretch",
        }
    }

    /// Parses a setting value.
    pub fn parse(s: &str) -> Option<Self> {
        [Self::Contain, Self::Cover, Self::Stretch]
            .into_iter()
            .find(|f| f.as_str() == s)
    }
}

/// Where the picture lands in the output, in output pixels (`x`, `y`, `w`, `h`), may
/// reach outside it for [`Fit::Cover`].
pub fn placement(picture: (u32, u32), output: (u32, u32), fit: Fit) -> (f64, f64, f64, f64) {
    let (pw, ph) = (f64::from(picture.0), f64::from(picture.1));
    let (ow, oh) = (f64::from(output.0), f64::from(output.1));
    if fit == Fit::Stretch || pw <= 0.0 || ph <= 0.0 {
        return (0.0, 0.0, ow, oh);
    }
    let scale = match fit {
        Fit::Contain => (ow / pw).min(oh / ph),
        _ => (ow / pw).max(oh / ph),
    };
    let (w, h) = (pw * scale, ph * scale);
    ((ow - w) / 2.0, (oh - h) / 2.0, w, h)
}

/// sRGB to linear light, by a 256-entry table built once per call site.
fn srgb_to_linear_table() -> [f32; 256] {
    let mut t = [0.0f32; 256];
    for (i, v) in t.iter_mut().enumerate() {
        let c = i as f32 / 255.0;
        *v = if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        };
    }
    t
}

fn linear_to_srgb(l: f32) -> u8 {
    let l = l.clamp(0.0, 1.0);
    let c = if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    };
    (c * 255.0).round() as u8
}

/// Fits a straight-alpha sRGB picture into an `output`-sized one (ADR-0171 decision 5).
/// Resampling is bilinear in premultiplied linear light, so edges and the `contain` bars
/// get no dark fringe; pixels outside the picture are transparent. The result is
/// straight-alpha sRGB again. Runs on the decode thread.
pub fn fit_picture(picture: &Picture, output: (u32, u32), fit: Fit) -> Picture {
    let (pw, ph, src) = (picture.0, picture.1, &picture.2);
    let (ow, oh) = output;
    let mut out = vec![0u8; ow as usize * oh as usize * 4];
    if pw == 0 || ph == 0 || ow == 0 || oh == 0 || src.len() != pw as usize * ph as usize * 4 {
        return (ow, oh, out);
    }
    if (pw, ph) == (ow, oh) {
        out.copy_from_slice(src);
        return (ow, oh, out);
    }
    let table = srgb_to_linear_table();
    // Premultiplied linear source, one f32 per channel.
    let lin: Vec<[f32; 4]> = src
        .chunks_exact(4)
        .map(|p| {
            let a = f32::from(p[3]) / 255.0;
            [
                table[usize::from(p[0])] * a,
                table[usize::from(p[1])] * a,
                table[usize::from(p[2])] * a,
                a,
            ]
        })
        .collect();
    let (x0, y0, w, h) = placement((pw, ph), output, fit);
    let sx = f64::from(pw) / w;
    let sy = f64::from(ph) / h;
    let at = |x: i64, y: i64| -> [f32; 4] {
        let x = x.clamp(0, i64::from(pw) - 1) as usize;
        let y = y.clamp(0, i64::from(ph) - 1) as usize;
        lin[y * pw as usize + x]
    };
    for oy in 0..oh {
        let cy = f64::from(oy) + 0.5;
        if cy < y0 || cy >= y0 + h {
            continue;
        }
        let fy = (cy - y0) * sy - 0.5;
        let iy = fy.floor();
        let ty = (fy - iy) as f32;
        for ox in 0..ow {
            let cx = f64::from(ox) + 0.5;
            if cx < x0 || cx >= x0 + w {
                continue;
            }
            let fx = (cx - x0) * sx - 0.5;
            let ix = fx.floor();
            let tx = (fx - ix) as f32;
            let (ix, iy) = (ix as i64, iy as i64);
            let (a, b, c, d) = (
                at(ix, iy),
                at(ix + 1, iy),
                at(ix, iy + 1),
                at(ix + 1, iy + 1),
            );
            let mut px = [0.0f32; 4];
            for k in 0..4 {
                let top = a[k] + (b[k] - a[k]) * tx;
                let bottom = c[k] + (d[k] - c[k]) * tx;
                px[k] = top + (bottom - top) * ty;
            }
            let i = (oy as usize * ow as usize + ox as usize) * 4;
            let alpha = px[3];
            if alpha <= 0.0 {
                continue;
            }
            out[i] = linear_to_srgb(px[0] / alpha);
            out[i + 1] = linear_to_srgb(px[1] / alpha);
            out[i + 2] = linear_to_srgb(px[2] / alpha);
            out[i + 3] = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
    (ow, oh, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> Entry {
        Entry {
            name: name.to_owned(),
            size: 0,
            modified: None,
        }
    }

    fn names(l: &Listing) -> Vec<&str> {
        l.entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn natural_order_compares_numbers_by_value() {
        let l = sorted_listing(
            [
                "10.png", "2.png", "1.png", "b.jpg", "A.jpg", "a.jpg", "02.png",
            ]
            .map(entry)
            .to_vec(),
        );
        assert_eq!(
            names(&l),
            [
                "1.png", "02.png", "2.png", "10.png", "A.jpg", "a.jpg", "b.jpg"
            ]
        );
    }

    #[test]
    fn the_cap_applies_after_sorting() {
        let all: Vec<Entry> = (0..MAX_FILES + 5)
            .rev()
            .map(|i| entry(&format!("{i}.png")))
            .collect();
        let l = sorted_listing(all);
        assert_eq!(l.entries.len(), MAX_FILES);
        assert_eq!(l.left_out, 5);
        assert_eq!(l.entries[0].name, "0.png");
        assert_eq!(
            l.entries[MAX_FILES - 1].name,
            format!("{}.png", MAX_FILES - 1)
        );
    }

    #[test]
    fn only_picture_names_count() {
        for yes in ["a.png", "b.JPG", "c.Jpeg", "x.y.png"] {
            assert!(is_picture_name(yes), "{yes}");
        }
        for no in [".hidden.png", "a.gif", "png", "a.png.txt", "a."] {
            assert!(!is_picture_name(no), "{no}");
        }
    }

    #[test]
    fn listing_skips_folders_links_and_hidden_files() {
        let dir = std::env::temp_dir().join(format!("rbs-slideshow-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub.png")).unwrap();
        for name in ["2.png", "10.jpg", ".x.png", "notes.txt"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("2.png"), dir.join("link.png")).unwrap();
        let l = list_folder(&dir).unwrap();
        assert_eq!(names(&l), ["2.png", "10.jpg"]);
        assert_eq!(l.entries[0].size, 1);
        assert_eq!(l.path(&dir, 1), Some(dir.join("10.jpg")));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(list_folder(&dir).is_err());
    }

    #[test]
    fn a_shuffle_pass_holds_every_picture_and_never_repeats_across_passes() {
        let mut s = Shuffler::new(7);
        let mut last = None;
        for _ in 0..500 {
            let pass = s.pass(5, last);
            let mut sorted = pass.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, [0, 1, 2, 3, 4]);
            assert_ne!(Some(pass[0]), last);
            last = pass.last().copied();
        }
        assert_eq!(s.pass(1, Some(0)), [0]);
        assert!(s.pass(0, None).is_empty());
    }

    #[test]
    fn placement_per_fit() {
        // A 200 x 100 picture into a 100 x 100 output.
        assert_eq!(
            placement((200, 100), (100, 100), Fit::Contain),
            (0.0, 25.0, 100.0, 50.0)
        );
        assert_eq!(
            placement((200, 100), (100, 100), Fit::Cover),
            (-50.0, 0.0, 200.0, 100.0)
        );
        assert_eq!(
            placement((200, 100), (100, 100), Fit::Stretch),
            (0.0, 0.0, 100.0, 100.0)
        );
        for f in [Fit::Contain, Fit::Cover, Fit::Stretch] {
            assert_eq!(Fit::parse(f.as_str()), Some(f));
        }
    }

    fn solid(w: u32, h: u32, px: [u8; 4]) -> Picture {
        (
            w,
            h,
            std::iter::repeat_n(px, (w * h) as usize)
                .flatten()
                .collect(),
        )
    }

    #[test]
    fn contain_leaves_transparent_bars_without_a_fringe() {
        let out = fit_picture(&solid(4, 2, [200, 40, 10, 255]), (4, 4), Fit::Contain);
        assert_eq!((out.0, out.1), (4, 4));
        let px = |x: usize, y: usize| &out.2[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4];
        assert_eq!(px(0, 0), [0, 0, 0, 0]);
        assert_eq!(px(3, 3), [0, 0, 0, 0]);
        // Inside: the colour is kept exactly (no darkening from the bars).
        assert_eq!(px(0, 1), [200, 40, 10, 255]);
        assert_eq!(px(3, 2), [200, 40, 10, 255]);
    }

    #[test]
    fn a_half_transparent_edge_keeps_its_colour() {
        // Red at full alpha next to a transparent black pixel: in straight alpha a
        // naive blend would darken the red; premultiplied keeps it red.
        let pic = (2, 1, vec![255, 0, 0, 255, 0, 0, 0, 0]);
        let out = fit_picture(&pic, (4, 1), Fit::Stretch);
        for p in out.2.chunks_exact(4).filter(|p| p[3] > 0) {
            assert_eq!(&p[..3], [255, 0, 0]);
        }
    }

    #[test]
    fn stretch_and_cover_fill_the_output() {
        for fit in [Fit::Cover, Fit::Stretch] {
            let out = fit_picture(&solid(3, 7, [9, 99, 199, 255]), (5, 5), fit);
            assert!(
                out.2.chunks_exact(4).all(|p| p == [9, 99, 199, 255]),
                "{fit:?}"
            );
        }
    }

    #[test]
    fn same_size_is_a_copy_and_bad_input_is_transparent() {
        let pic = solid(2, 2, [1, 2, 3, 4]);
        assert_eq!(fit_picture(&pic, (2, 2), Fit::Contain), pic);
        let bad = (2, 2, vec![1, 2, 3]);
        assert_eq!(fit_picture(&bad, (1, 1), Fit::Contain).2, [0, 0, 0, 0]);
    }
}
