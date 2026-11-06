//! The installed panic hook, in its own test binary: the hook is process-global.

#![allow(clippy::unwrap_used, clippy::expect_used)] // Tests fail by panicking.

use std::fs;

#[test]
fn hook_reports_only_on_threads_that_asked() {
    let dir = std::env::temp_dir().join(format!("rbs-crash-hook-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let session = rbs_crash::install(&dir).unwrap();
    assert!(session.unclean().is_empty());

    // A thread that did not ask (a real-time thread, say): no report.
    std::thread::spawn(|| rbs_crash::contain(|| -> u8 { panic!("on a worker") }))
        .join()
        .unwrap()
        .unwrap_err();
    let reports = || {
        fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|x| x == "txt"))
            .collect::<Vec<_>>()
    };
    assert!(reports().is_empty());

    rbs_crash::report_panics_on_this_thread();
    let caught = rbs_crash::contain(|| -> u8 { panic!("in a callback") }).unwrap_err();
    assert_eq!(caught.message, "in a callback");
    let written = reports();
    assert_eq!(written.len(), 1);
    let text = fs::read_to_string(&written[0]).unwrap();
    assert!(text.contains("\nKind: caught\n"), "{text}");
    assert!(text.contains("\nMessage: in a callback\n"), "{text}");

    session.end_clean();
    fs::remove_dir_all(&dir).unwrap();
}
