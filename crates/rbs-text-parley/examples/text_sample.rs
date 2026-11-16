//! Draws a few text blocks with the system's fonts into one PNG, to look at the
//! renderer by eye: `cargo run -p rbs-text-parley --example text_sample -- out.png`.

use std::fs::File;
use std::io::BufWriter;

use rbs_text::{Align, Outline, Shadow, TextBlock, TextRenderer, VAlign};
use rbs_text_parley::ParleyRenderer;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "text-sample.png".to_owned());
    let r = ParleyRenderer::system();
    let (w, h) = (1280u32, 720u32);
    let mut sheet = vec![0u8; (w * h * 4) as usize];
    // A dark checker so transparency shows.
    for y in 0..h {
        for x in 0..w {
            let v = if ((x / 16) + (y / 16)) % 2 == 0 {
                40
            } else {
                52
            };
            let i = ((y * w + x) * 4) as usize;
            sheet[i..i + 4].copy_from_slice(&[v, v, v + 8, 255]);
        }
    }
    let mut blocks = Vec::new();

    let mut title = TextBlock::new("Merhaba Dünya! İĞÜŞÖÇ ığüşöç", [1200, 130]);
    title.size = 84.0;
    title.weight = 700;
    title.fill.color = [255, 214, 64, 255];
    title.outline = Some(Outline {
        width: 5.0,
        color: [10, 10, 10, 255],
    });
    title.shadow = Some(Shadow {
        color: [0, 0, 0, 180],
        offset: [6.0, 8.0],
        blur: 8.0,
    });
    blocks.push(((40, 20), title));

    let mut plate = TextBlock::new("Salih Cantekin\nRBS · Rust Broadcasting", [620, 170]);
    plate.size = 44.0;
    plate.align = Align::Left;
    plate.background = Some([30, 70, 160, 230]);
    plate.padding = 24.0;
    blocks.push(((40, 190), plate));

    let mut clock = TextBlock::new("20:45:07", [560, 170]);
    clock.size = 120.0;
    clock.weight = 600;
    clock.tabular_figures = true;
    clock.fill.gradient = Some(([120, 200, 255, 255], 0.0));
    blocks.push(((690, 190), clock));

    let mut wrap = TextBlock::new(
        "Uzun bir metin kutuya sığmazsa yazı küçülür (Fit). العربية من اليمين إلى اليسار",
        [560, 300],
    );
    wrap.size = 64.0;
    wrap.valign = VAlign::Top;
    wrap.background = Some([0, 0, 0, 120]);
    wrap.padding = 16.0;
    blocks.push(((40, 390), wrap));

    let mut ellipsis = TextBlock::new("Kontur ve gölge, sistem fontu", [560, 300]);
    ellipsis.size = 56.0;
    ellipsis.italic = true;
    ellipsis.outline = Some(Outline {
        width: 3.0,
        color: [200, 40, 60, 255],
    });
    ellipsis.shadow = Some(Shadow {
        color: [0, 0, 0, 255],
        offset: [0.0, 0.0],
        blur: 16.0,
    });
    blocks.push(((690, 390), ellipsis));

    for ((bx, by), block) in blocks {
        let t = std::time::Instant::now();
        let (p, _) = r.render(&block)?;
        eprintln!("{}x{} in {:?}", p.width(), p.height(), t.elapsed());
        for y in 0..p.height() {
            for x in 0..p.width() {
                let Some(px) = p.pixel(x, y) else { continue };
                let i = (((by + y) * w + bx + x) * 4) as usize;
                let a = u32::from(px[3]);
                for k in 0..3 {
                    let d = u32::from(sheet[i + k]);
                    sheet[i + k] = ((u32::from(px[k]) * a + d * (255 - a)) / 255) as u8;
                }
            }
        }
    }
    let mut enc = png::Encoder::new(BufWriter::new(File::create(&out)?), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&sheet)?;
    Ok(())
}
