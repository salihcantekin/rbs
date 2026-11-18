//! The adapter against real archives, a real directory and a local HTTP server.
//! Timeouts here are short real-time bounds on a server that never answers.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Duration;

use rbs_add_ons::{AddOns, Fetch, FetchError, InstallControl, InstallError, Response, parse_entry};
use rbs_add_ons_sys::{SysFetch, SysFs, SysUnpack};
use rbs_core::time::TestClock;
use sha2::{Digest, Sha256};

fn tar_bytes(files: &[(&str, &[u8])], symlink: Option<(&str, &str)>) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, data) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        builder.append_data(&mut header, name, *data).unwrap();
    }
    if let Some((name, target)) = symlink {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        builder.append_link(&mut header, name, target).unwrap();
    }
    builder.into_inner().unwrap()
}

fn gz(bytes: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

fn bz2(bytes: &[u8]) -> Vec<u8> {
    let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::fast());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

struct Serve(Vec<u8>);

impl Fetch for Serve {
    fn get(&self, _url: &str) -> Result<Response, FetchError> {
        Ok(Response::Body(Box::new(std::io::Cursor::new(
            self.0.clone(),
        ))))
    }
}

fn entry(archive: &[u8], format: &str) -> rbs_add_ons::Entry {
    let sha: String = Sha256::digest(archive)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let text = format!(
        r#"
id = "org.rbs.browser-engine"
kind = "runtime"
name = "Browser engine"
summary = "Test."
version = "1.0.0"
hosts = ["cdn.example.org"]
notices = "https://cdn.example.org/n"
[platform.t]
url = "https://cdn.example.org/a"
size = {}
sha256 = "{sha}"
unpacked_size = 100000
format = "{format}"
strip = 1
library_dir = "Release"
resources_dir = "Resources"
required = ["libcef.so", "icudtl.dat"]
"#,
        archive.len()
    );
    parse_entry(&text, "t").unwrap().unwrap()
}

fn install(
    archive: Vec<u8>,
    format: &str,
) -> (
    tempfile::TempDir,
    Result<rbs_add_ons::Installed, InstallError>,
) {
    let tmp = tempfile::tempdir().unwrap();
    let addons = AddOns::new(tmp.path().join("add-ons"), Arc::new(SysFs));
    let e = entry(&archive, format);
    let cancel = AtomicBool::new(false);
    let clock = TestClock::new();
    let result = addons.install(
        &e,
        &Serve(archive),
        &SysUnpack,
        InstallControl {
            cancel: &cancel,
            clock: &clock,
            progress: &mut |_| {},
        },
    );
    (tmp, result)
}

const RUNTIME: &[(&str, &[u8])] = &[
    ("cef/Release/libcef.so", b"lib"),
    ("cef/Resources/icudtl.dat", b"icu"),
    ("cef/Resources/locales/en-US.pak", b"pak"),
];

#[test]
fn a_tar_gz_and_a_tar_bz2_install_for_real() {
    for (bytes, format) in [
        (gz(&tar_bytes(RUNTIME, None)), "tar.gz"),
        (bz2(&tar_bytes(RUNTIME, None)), "tar.bz2"),
    ] {
        let (_tmp, result) = install(bytes, format);
        let done = result.unwrap();
        assert_eq!(
            std::fs::read(done.library_dir.join("icudtl.dat")).unwrap(),
            b"icu"
        );
        assert!(done.resources_dir.join("locales/en-US.pak").is_file());
        assert!(done.path.join(".complete").is_file());
    }
}

#[test]
fn an_escaping_link_in_a_real_tar_is_refused() {
    let bytes = gz(&tar_bytes(RUNTIME, Some(("cef/Release/l", "../../../etc"))));
    let (_tmp, result) = install(bytes, "tar.gz");
    assert!(
        matches!(result, Err(InstallError::UnsafeArchive(_))),
        "{result:?}"
    );
}

#[test]
fn a_corrupt_archive_is_refused() {
    let mut bytes = gz(&tar_bytes(RUNTIME, None));
    bytes.truncate(bytes.len() / 2);
    let (_tmp, result) = install(bytes, "tar.gz");
    assert!(
        matches!(result, Err(InstallError::UnsafeArchive(_))),
        "{result:?}"
    );
}

/// Serves one connection with `reply`, then keeps it open without sending more.
fn server(reply: &'static [u8]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(reply);
            // Hold the connection; the client's timeout ends the test.
            let _ = stream.read(&mut buf);
        }
    });
    format!("http://{addr}/file")
}

fn fast() -> SysFetch {
    SysFetch::with_timeouts(Duration::from_secs(2), Duration::from_millis(300))
}

#[test]
fn a_body_is_returned() {
    let url = server(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello");
    let Response::Body(mut body) = fast().get(&url).unwrap() else {
        panic!("expected a body");
    };
    let mut text = String::new();
    body.read_to_string(&mut text).unwrap();
    assert_eq!(text, "hello");
}

#[test]
fn a_redirect_is_returned_not_followed() {
    let url = server(b"HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0\r\n\r\n");
    match fast().get(&url).unwrap() {
        Response::Redirect(to) => assert!(to.ends_with("/next"), "{to}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_error_status_is_a_network_error() {
    let url = server(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
    assert!(matches!(fast().get(&url), Err(FetchError::Network(_))));
}

#[test]
fn a_body_that_stops_arriving_is_stalled() {
    let url = server(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc");
    let Response::Body(mut body) = fast().get(&url).unwrap() else {
        panic!("expected a body");
    };
    let mut all = Vec::new();
    let err = body.read_to_end(&mut all).unwrap_err();
    let stalled = err
        .get_ref()
        .and_then(|e| e.downcast_ref::<FetchError>())
        .is_some_and(|f| *f == FetchError::Stalled);
    assert!(stalled, "{err:?}");
}

#[test]
fn a_server_that_never_answers_times_out() {
    let url = server(b"");
    assert!(matches!(fast().get(&url), Err(FetchError::Network(_))));
}
