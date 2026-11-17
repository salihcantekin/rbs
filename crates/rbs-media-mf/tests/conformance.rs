//! Conformance of the Media Foundation adapter (ADR-0108 decision 13), run on Windows.
//!
//! `fixtures/clip.mp4`: H.264 Main 160x90 (coded 160x96, so the display aperture must crop),
//! 10 fps, 5 frames, red with a green top-left 80x45, and 0.5 s of 48 kHz stereo AAC.
//! A machine without the H.264 or AAC decoder (Windows N without the Media Feature
//! Pack) skips with a reason instead of failing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use rbs_media_decode::{DecodeError, MediaDecoderFactory};
use rbs_media_mf::MfMediaFactory;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

#[cfg(not(windows))]
#[test]
fn other_platforms_refuse_as_unsupported() {
    let refused = MfMediaFactory::new().open(&fixture("clip.mp4"));
    assert!(matches!(refused, Err(DecodeError::Unsupported(_))));
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::time::Duration;

    use rbs_media_decode::{MediaDecoder, PictureData, PictureLayout, PictureSample};

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    fn open() -> Option<Box<dyn MediaDecoder>> {
        match MfMediaFactory::new().open(&fixture("clip.mp4")) {
            Ok(decoder) => Some(decoder),
            Err(DecodeError::Unsupported(why)) => {
                eprintln!("skipped: {why}");
                None
            }
            Err(e) => panic!("open: {e}"),
        }
    }

    fn near(a: [u8; 4], b: [u8; 4]) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 24)
    }

    fn pixel(sample: &PictureSample, x: usize, y: usize) -> [u8; 4] {
        let PictureData::System { layout, bytes, .. } = sample.picture.data() else {
            panic!("a system picture");
        };
        assert_eq!(*layout, PictureLayout::Rgba8);
        let w = sample.picture.width as usize;
        let i = (y * w + x) * 4;
        [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]
    }

    /// A coarse view of the picture for a failure message: the colour at every 8th
    /// pixel of every 6th row.
    fn map(sample: &PictureSample) -> String {
        let (w, h) = (
            sample.picture.width as usize,
            sample.picture.height as usize,
        );
        let mut out = String::new();
        for y in (0..h).step_by(6) {
            for x in (0..w).step_by(8) {
                let [r, g, b, _] = pixel(sample, x, y);
                out.push_str(&format!("{r:3},{g:3},{b:3} "));
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn pictures_are_cropped_upright_rgba_with_stamps() {
        let Some(mut dec) = open() else { return };
        assert!(dec.has_audio());
        let length = dec.duration().expect("a stated length");
        assert!(length >= ms(450) && length <= ms(550), "{length:?}");
        let mut pictures = Vec::new();
        while let Some(sample) = dec.next_picture().unwrap() {
            pictures.push(sample);
            assert!(pictures.len() < 50, "the stream must end");
        }
        assert_eq!(pictures.len(), 5);
        for (i, p) in pictures.iter().enumerate() {
            assert_eq!((p.picture.width, p.picture.height), (160, 90));
            assert_eq!(p.pts, Some(ms(100 * i as u64)), "picture {i}");
            assert_eq!(p.frame_duration, Some(ms(100)), "picture {i}");
            let green = pixel(p, 8, 8);
            let red = pixel(p, 150, 80);
            assert!(
                near(green, [0, 255, 0, 255]),
                "picture {i} top-left {green:?}\n{}",
                map(p)
            );
            assert!(
                near(red, [255, 0, 0, 255]),
                "picture {i} bottom-right {red:?}\n{}",
                map(p)
            );
        }
    }

    #[test]
    fn a_seek_moves_both_streams() {
        let Some(mut dec) = open() else { return };
        dec.seek(ms(300)).unwrap();
        let first = dec
            .next_picture()
            .unwrap()
            .expect("a picture after the seek");
        // The picture at or before the cursor (a keyframe at most 400 ms earlier).
        assert!(first.pts.unwrap() <= ms(300));
        let mut last = first.pts.unwrap();
        while let Some(p) = dec.next_picture().unwrap() {
            last = p.pts.unwrap();
        }
        assert_eq!(last, ms(400));
        let sound = dec.next_sound().unwrap().expect("sound after the seek");
        assert!(sound.pts.unwrap() <= ms(300) && sound.pts.unwrap() >= ms(200));
    }

    #[test]
    fn sound_is_interleaved_float_of_the_whole_clip() {
        let Some(mut dec) = open() else { return };
        let mut frames = 0usize;
        let mut chunks = 0;
        let mut loudest = 0.0f32;
        while let Some(sound) = dec.next_sound().unwrap() {
            assert_eq!(sound.pcm.sample_rate, 48_000);
            assert_eq!(sound.pcm.channels, 2);
            assert_eq!(sound.pcm.samples.len() % 2, 0);
            loudest = sound
                .pcm
                .samples
                .iter()
                .fold(loudest, |m, s| m.max(s.abs()));
            frames += sound.pcm.samples.len() / 2;
            chunks += 1;
            assert!(chunks < 500, "the stream must end");
        }
        // The fixture's tone peaks near 0.1 to 0.14; a single AAC frame may stay below
        // 0.1, so the level is checked over the whole clip.
        assert!(loudest > 0.05, "the sound is silent: peak {loudest}");
        // 0.5 s, give or take two AAC frames: Media Foundation returns every coded
        // frame, the encoder's priming and the end padding included (25 x 1024 = 25600
        // for the fixture), where AVFoundation trims them.
        assert!(
            (24_000 - 2048..=24_000 + 2048).contains(&frames),
            "{frames}"
        );
    }

    /// The first DXGI adapter's LUID and a D3D11 device on it, to read native pictures.
    fn first_adapter() -> Option<(u64, ::windows::Win32::Graphics::Direct3D11::ID3D11Device)> {
        use ::windows::Win32::Foundation::HMODULE;
        use ::windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
        use ::windows::Win32::Graphics::Direct3D11::{
            D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11CreateDevice,
        };
        use ::windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
        // SAFETY: plain factory, adapter and device calls with valid out pointers.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            let adapter = factory.EnumAdapters1(0).ok()?;
            let luid = adapter.GetDesc1().ok()?.AdapterLuid;
            let mut device = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .ok()?;
            Some((
                (u64::from(luid.HighPart as u32) << 32) | u64::from(luid.LowPart),
                device?,
            ))
        }
    }

    /// The `B, G, R, A` pixel at (`x`, `y`) of a shared texture, read back through a
    /// staging copy.
    fn shared_pixel(
        device: &::windows::Win32::Graphics::Direct3D11::ID3D11Device,
        handle: u64,
        x: u32,
        y: u32,
    ) -> [u8; 4] {
        use ::windows::Win32::Foundation::HANDLE;
        use ::windows::Win32::Graphics::Direct3D11::{
            D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
            D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, ID3D11Device1, ID3D11Texture2D,
        };
        use ::windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
        };
        use ::windows::core::Interface;
        // SAFETY: the handle is open while the picture's lease lives (the caller holds
        // the picture); every out pointer is valid and the mapping is read inside
        // Map/Unmap.
        unsafe {
            let device1: ID3D11Device1 = device.cast().unwrap();
            let shared: ID3D11Texture2D = device1
                .OpenSharedResource1(HANDLE(handle as usize as *mut _))
                .unwrap();
            let desc = D3D11_TEXTURE2D_DESC {
                Width: 1,
                Height: 1,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .unwrap();
            let staging = staging.unwrap();
            let context = device.GetImmediateContext().unwrap();
            let region = D3D11_BOX {
                left: x,
                top: y,
                front: 0,
                right: x + 1,
                bottom: y + 1,
                back: 1,
            };
            context.CopySubresourceRegion(&staging, 0, 0, 0, 0, &shared, 0, Some(&region));
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let bytes = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), 4);
            let out = [bytes[0], bytes[1], bytes[2], bytes[3]];
            context.Unmap(&staging, 0);
            out
        }
    }

    #[test]
    fn native_pictures_are_shared_bgra_textures_with_stamps() {
        use rbs_core::media::{FramePayload, NativeSurface, PixelFormat};
        use rbs_media_decode::DecodeTarget;

        let Some((luid, device)) = first_adapter() else {
            eprintln!("skipped: no DXGI adapter");
            return;
        };
        let mut dec = match MfMediaFactory::new()
            .open_for(&fixture("clip.mp4"), &DecodeTarget::adapter(luid))
        {
            Ok(decoder) => decoder,
            Err(DecodeError::Unsupported(why)) => {
                eprintln!("skipped: {why}");
                return;
            }
            Err(e) => panic!("open: {e}"),
        };
        let mut pictures = Vec::new();
        while let Some(sample) = dec.next_picture().unwrap() {
            pictures.push(sample);
            assert!(pictures.len() < 50, "the stream must end");
        }
        assert_eq!(pictures.len(), 5);
        if !pictures
            .iter()
            .any(|p| matches!(p.picture.data(), PictureData::Native(_)))
        {
            eprintln!("skipped: this machine decodes the fixture off the GPU");
            return;
        }
        for (i, p) in pictures.iter().enumerate() {
            assert_eq!(p.pts, Some(ms(100 * i as u64)), "picture {i}");
            assert_eq!(p.frame_duration, Some(ms(100)), "picture {i}");
            let PictureData::Native(frame) = p.picture.data() else {
                panic!("picture {i} is not native");
            };
            assert_eq!(frame.format().pixel_format, PixelFormat::Bgra8);
            assert_eq!(
                (frame.format().size.width, frame.format().size.height),
                (160, 90)
            );
            let FramePayload::Native {
                surface:
                    NativeSurface::D3dSharedHandle {
                        handle,
                        nt,
                        adapter_luid,
                        ..
                    },
                ..
            } = *frame.payload()
            else {
                panic!("picture {i} is not a shared handle");
            };
            assert!(nt);
            assert_eq!(adapter_luid, luid);
            let green = shared_pixel(&device, handle, 8, 8);
            let red = shared_pixel(&device, handle, 150, 80);
            assert!(
                near(green, [0, 255, 0, 255]),
                "picture {i} top-left {green:?}"
            );
            assert!(
                near(red, [0, 0, 255, 255]),
                "picture {i} bottom-right {red:?}"
            );
        }
        // Released pictures free their slots; the seek and the next reads still work.
        drop(pictures);
        dec.seek(ms(300)).unwrap();
        // The picture at or before the cursor: Media Foundation may start at the
        // keyframe before it (invariant 3), here the first picture.
        let mut last = None;
        while let Some(p) = dec.next_picture().unwrap() {
            assert!(matches!(p.picture.data(), PictureData::Native(_)));
            if last.is_none() {
                assert!(p.pts.unwrap() <= ms(300));
            }
            last = p.pts;
        }
        assert_eq!(last, Some(ms(400)));
    }

    #[test]
    fn a_file_that_is_no_media_is_refused() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert!(MfMediaFactory::new().open(&manifest).is_err());
    }
}
