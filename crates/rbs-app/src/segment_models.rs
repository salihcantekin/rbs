//! The model folder of the person mask backend (ADR-0179 decision 8, spec
//! `person-mask-port.md` rule 10): networks are data the user puts in
//! `<config>/models/person-mask/<name>.onnx`. A model is lent only when its size and
//! SHA-256 match the list below; a path from a scene document is never a model.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rbs_segment_ort::ModelSource;
use sha2::{Digest, Sha256};

/// One known model: its name, size in bytes and SHA-256 (lowercase hex), with its source
/// and licence (copied to the third-party notices).
pub struct Known {
    /// The model's name; the file is `<name>.onnx`.
    pub name: &'static str,
    /// The file's size in bytes.
    pub size: u64,
    /// The file's SHA-256, lowercase hex.
    pub sha256: &'static str,
}

/// The models this build accepts. MediaPipe selfie segmentation (Apache-2.0,
/// onnx-community/mediapipe_selfie_segmentation), MODNet (Apache-2.0, Xenova/modnet) and
/// Robust Video Matting MobileNetV3 fp32 (GPL-3.0, PeterL1n/RobustVideoMatting v1.0.0).
pub const KNOWN: [Known; 3] = [
    Known {
        name: "mediapipe-selfie-256",
        size: 462_352,
        sha256: "3241ac4ad8aa35bdaf33946776db29f7c283a413aa0b0dacb9483594b4531aad",
    },
    Known {
        name: "modnet-webcam",
        size: 25_888_640,
        sha256: "07c308cf0fc7e6e8b2065a12ed7fc07e1de8febb7dc7839d7b7f15dd66584df9",
    },
    Known {
        name: "rvm-mobilenetv3-fp32",
        size: 14_975_696,
        sha256: "88d4531297118f595bf2fd60f6f566aec2e559393802d1f436c380f0cbbd2828",
    },
];

/// The folder of the models.
pub struct ModelFolder {
    dir: PathBuf,
    known: &'static [Known],
}

impl ModelFolder {
    /// The folder `dir`, accepting the models of [`KNOWN`].
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self::with_known(dir, &KNOWN)
    }

    /// The folder `dir`, accepting `known` (for tests).
    pub fn with_known(dir: impl Into<PathBuf>, known: &'static [Known]) -> Self {
        Self {
            dir: dir.into(),
            known,
        }
    }

    fn path(&self, name: &str) -> Option<PathBuf> {
        // Only a listed name is a model: no path, no `..`.
        self.known
            .iter()
            .any(|k| k.name == name)
            .then(|| self.dir.join(format!("{name}.onnx")))
    }
}

impl ModelSource for ModelFolder {
    fn exists(&self, name: &str) -> bool {
        self.path(name).is_some_and(|p| p.is_file())
    }

    fn load(&self, name: &str) -> Option<Arc<[u8]>> {
        let known = self.known.iter().find(|k| k.name == name)?;
        let bytes = std::fs::read(self.path(name)?).ok()?;
        if bytes.len() as u64 != known.size {
            return None;
        }
        let digest = Sha256::digest(&bytes);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        (hex == known.sha256).then(|| Arc::from(bytes))
    }
}

/// `<config>/models/person-mask`.
pub fn models_dir(config: &Path) -> PathBuf {
    config.join("models").join("person-mask")
}

/// The ONNX Runtime library file: `RBS_ONNXRUNTIME` if set, else
/// `<config>/runtime/onnxruntime/<library name>` (the DirectML build on Windows).
pub fn runtime_file(config: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("RBS_ONNXRUNTIME") {
        return PathBuf::from(p);
    }
    let name = if cfg!(windows) {
        "onnxruntime.dll"
    } else if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    };
    config.join("runtime").join("onnxruntime").join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The digest of the bytes b"abc".
    static TEST: [Known; 1] = [Known {
        name: "test-model",
        size: 3,
        sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    }];

    fn dir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("rbs-segment-models-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_model_with_the_right_size_and_hash_is_lent() {
        let d = dir("ok");
        std::fs::write(d.join("test-model.onnx"), b"abc").unwrap();
        let folder = ModelFolder::with_known(&d, &TEST);
        assert!(folder.exists("test-model"));
        assert_eq!(&*folder.load("test-model").unwrap(), b"abc");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_changed_file_a_wrong_size_and_an_unknown_name_are_refused() {
        let d = dir("bad");
        std::fs::write(d.join("test-model.onnx"), b"abd").unwrap();
        std::fs::write(d.join("other.onnx"), b"abc").unwrap();
        let folder = ModelFolder::with_known(&d, &TEST);
        assert!(folder.load("test-model").is_none(), "wrong hash");
        std::fs::write(d.join("test-model.onnx"), b"abcd").unwrap();
        assert!(folder.load("test-model").is_none(), "wrong size");
        assert!(!folder.exists("other") && folder.load("other").is_none());
        assert!(!folder.exists("../other") && folder.load("../other").is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_runtime_sits_in_the_config_folder() {
        let p = runtime_file(Path::new("/c"));
        if std::env::var_os("RBS_ONNXRUNTIME").is_none() {
            assert!(p.starts_with("/c/runtime/onnxruntime") || p.starts_with("/c\\runtime"));
        }
    }
}
