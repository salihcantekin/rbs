//! Program preview item. Qt renders with its own graphics device, so this
//! prototype copies engine frames through the CPU (the measured fallback,
//! not the zero-copy default required by P4).

use std::pin::Pin;
use std::sync::Mutex;

use cxx_qt_lib::{QImage, QImageFormat, QRect};

/// Latest program frame (RGBA8, width, height). Written on the GUI thread,
/// read in `paint`, which Qt Quick calls on its render thread.
pub static FRAME: Mutex<(Vec<u8>, i32, i32)> = Mutex::new((Vec::new(), 0, 0));

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!(<QtQuick/QQuickPaintedItem>);
        type QQuickPaintedItem;
        include!("cxx-qt-lib/qpainter.h");
        type QPainter = cxx_qt_lib::QPainter;
        include!("cxx-qt-lib/qrect.h");
        type QRect = cxx_qt_lib::QRect;
    }

    // SAFETY: the inherited methods are plain QQuickItem getters and update().
    unsafe extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[base = QQuickPaintedItem]
        #[qproperty(i32, frame)]
        type ProgramItem = super::ProgramItemRust;

        #[cxx_override]
        unsafe fn paint(self: Pin<&mut ProgramItem>, painter: *mut QPainter);

        #[inherit]
        fn update(self: Pin<&mut ProgramItem>, rect: &QRect);

        #[inherit]
        fn width(self: &ProgramItem) -> f64;

        #[inherit]
        fn height(self: &ProgramItem) -> f64;
    }

    impl cxx_qt::Constructor<()> for ProgramItem {}
}

#[derive(Default)]
pub struct ProgramItemRust {
    frame: i32,
}

impl cxx_qt::Initialize for qobject::ProgramItem {
    fn initialize(self: Pin<&mut Self>) {
        self.on_frame_changed(|item| item.update(&QRect::default()))
            .release();
    }
}

impl qobject::ProgramItem {
    unsafe fn paint(self: Pin<&mut Self>, painter: *mut qobject::QPainter) {
        let (w, h) = (self.width(), self.height());
        {
            let Ok(f) = FRAME.lock() else { return };
            if f.1 == 0 {
                return;
            }
            // Copy into a QImage owned by Qt (second CPU copy of the frame).
            // SAFETY: the buffer holds exactly width * height RGBA8 pixels.
            let img = unsafe {
                QImage::from_raw_bytes(f.0.clone(), f.1, f.2, QImageFormat::Format_RGBA8888)
            };
            // Letterbox to the item, keeping the aspect ratio.
            let s = (w / f.1 as f64).min(h / f.2 as f64);
            let (dw, dh) = ((f.1 as f64 * s) as i32, (f.2 as f64 * s) as i32);
            let rect = QRect::new(((w as i32) - dw) / 2, ((h as i32) - dh) / 2, dw, dh);
            // SAFETY: Qt passes a valid painter for the duration of paint().
            if let Some(p) = unsafe { painter.as_mut() } {
                let p = unsafe { Pin::new_unchecked(p) };
                p.draw_image(&rect, &img);
            }
        }
    }
}
