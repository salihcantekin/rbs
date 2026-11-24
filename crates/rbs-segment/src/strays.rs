//! Removing stray regions from a matte.
//!
//! A network sometimes takes an object in the room (a hat on a shelf, a lamp) for a
//! second person. Such a region is not joined to the person. [`StrayFilter`] keeps the
//! largest region of a matte and every region that is at least a tenth as large (a
//! second person, a hand that is cut off from the arm) and sets the rest to zero. It
//! runs on the backend's own thread over the small matte, never on a render path.

/// Coverage from which a pixel belongs to a region (soft edges join their region).
const FLOOR: u8 = 32;
/// A region smaller than the largest one divided by this is dropped.
const SMALL: u32 = 10;

/// Reusable scratch for [`StrayFilter::apply`]; allocates only when the matte grows.
#[derive(Debug, Default)]
pub struct StrayFilter {
    labels: Vec<u32>,
    stack: Vec<u32>,
    areas: Vec<u32>,
    /// Per label: the region reaches the bottom row (a body comes in from below).
    grounded: Vec<bool>,
}

impl StrayFilter {
    /// A filter with no scratch yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Zeroes the small regions of `plane` (`width` x `height`, one coverage byte per
    /// pixel). A plane of another length is left alone.
    pub fn apply(&mut self, plane: &mut [u8], width: u32, height: u32) {
        let (w, h) = (width as usize, height as usize);
        if w == 0 || h == 0 || plane.len() != w * h {
            return;
        }
        self.labels.clear();
        self.labels.resize(plane.len(), 0);
        self.areas.clear();
        self.areas.push(0); // label 0: not in a region
        self.grounded.clear();
        self.grounded.push(false);
        for start in 0..plane.len() {
            if plane[start] < FLOOR || self.labels[start] != 0 {
                continue;
            }
            let label = self.areas.len() as u32;
            self.areas.push(0);
            self.grounded.push(false);
            self.labels[start] = label;
            self.stack.push(start as u32);
            while let Some(at) = self.stack.pop() {
                let at = at as usize;
                self.areas[label as usize] += 1;
                let (x, y) = (at % w, at / w);
                if y + 1 == h {
                    self.grounded[label as usize] = true;
                }
                let mut visit = |next: usize| {
                    if plane[next] >= FLOOR && self.labels[next] == 0 {
                        self.labels[next] = label;
                        self.stack.push(next as u32);
                    }
                };
                if x > 0 {
                    visit(at - 1);
                }
                if x + 1 < w {
                    visit(at + 1);
                }
                if y > 0 {
                    visit(at - w);
                }
                if y + 1 < h {
                    visit(at + w);
                }
            }
        }
        let largest = self.areas.iter().copied().max().unwrap_or(0);
        if self.areas.len() <= 2 {
            return;
        }
        for (pixel, label) in plane.iter_mut().zip(&self.labels) {
            let l = *label as usize;
            if *label != 0 && !self.grounded[l] && self.areas[l] * SMALL < largest {
                *pixel = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(plane: &mut [u8], w: usize, x: usize, y: usize, bw: usize, bh: usize) {
        for row in y..y + bh {
            for col in x..x + bw {
                plane[row * w + col] = 255;
            }
        }
    }

    #[test]
    fn a_small_separate_region_is_removed_and_the_person_stays() {
        let (w, h) = (40usize, 30usize);
        let mut plane = vec![0u8; w * h];
        block(&mut plane, w, 2, 2, 20, 26); // the person
        block(&mut plane, w, 30, 4, 3, 3); // a hat on a shelf
        StrayFilter::new().apply(&mut plane, w as u32, h as u32);
        assert_eq!(plane[10 * w + 10], 255);
        assert_eq!(plane[5 * w + 31], 0);
    }

    #[test]
    fn a_second_region_of_some_size_stays() {
        let (w, h) = (40usize, 30usize);
        let mut plane = vec![0u8; w * h];
        block(&mut plane, w, 2, 2, 16, 20);
        block(&mut plane, w, 24, 4, 10, 12); // another person
        StrayFilter::new().apply(&mut plane, w as u32, h as u32);
        assert_eq!(plane[8 * w + 28], 255);
    }

    #[test]
    fn a_hand_joined_to_the_arm_stays() {
        let (w, h) = (40usize, 30usize);
        let mut plane = vec![0u8; w * h];
        block(&mut plane, w, 2, 2, 16, 26);
        block(&mut plane, w, 18, 10, 8, 2); // an arm reaching out, joined
        block(&mut plane, w, 26, 8, 2, 6); // the hand, joined to the arm
        StrayFilter::new().apply(&mut plane, w as u32, h as u32);
        assert_eq!(plane[9 * w + 27], 255);
    }

    #[test]
    fn a_small_region_touching_the_bottom_row_stays() {
        let (w, h) = (40usize, 30usize);
        let mut plane = vec![0u8; w * h];
        block(&mut plane, w, 2, 2, 20, 20);
        block(&mut plane, w, 30, 27, 3, 3); // a hand coming in from below
        StrayFilter::new().apply(&mut plane, w as u32, h as u32);
        assert_eq!(plane[28 * w + 31], 255);
    }

    #[test]
    fn an_empty_or_wrongly_sized_plane_is_left_alone() {
        let mut filter = StrayFilter::new();
        let mut empty = vec![0u8; 12];
        filter.apply(&mut empty, 4, 3);
        assert!(empty.iter().all(|&v| v == 0));
        let mut odd = vec![255u8; 5];
        filter.apply(&mut odd, 4, 3);
        assert!(odd.iter().all(|&v| v == 255));
    }
}
