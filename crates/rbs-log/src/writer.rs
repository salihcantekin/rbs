//! The writer: what the `rbs-log` thread does with each line (spec
//! `diagnostics-log.md`, "Writer"). Kept free of threads and clocks so tests drive it.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use rbs_core::log::{Area, Level};
use rbs_crash::LogRedactor;

use crate::files::{self, Limits, Part};
use crate::time::utc_millis;

/// Lines the memory ring keeps.
pub const RING_LINES: usize = 500;

/// One line as the sink hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// When it was logged.
    pub time: SystemTime,
    /// How serious it is.
    pub level: Level,
    /// Its area.
    pub area: Area,
    /// The calling thread's name, or `?`.
    pub thread: String,
    /// The call site.
    pub file: &'static str,
    /// The call site's line.
    pub line: u32,
    /// The message, not yet redacted.
    pub text: String,
}

/// The last 500 written lines, shared with [`crate::Logger::recent`].
pub type Ring = Arc<Mutex<VecDeque<String>>>;

/// Writes lines to the current file, rotating, collapsing repeats and filling the ring.
pub struct Writer {
    dir: PathBuf,
    prefix: String,
    limits: Limits,
    part: Part,
    out: Option<BufWriter<File>>,
    written: u64,
    redactor: LogRedactor,
    ring: Ring,
    last: Option<(&'static str, u32, String)>,
    repeats: u64,
    echo: bool,
}

impl Writer {
    /// Opens the run's first file in `dir` (pruning old runs first) and writes `header`.
    pub fn open(
        dir: &Path,
        prefix: &str,
        part: Part,
        limits: Limits,
        home: Option<String>,
        header: &[String],
        echo: bool,
    ) -> std::io::Result<Writer> {
        std::fs::create_dir_all(dir)?;
        files::prune(dir, prefix, &part, &limits);
        let file = files::create_locked(dir, &files::name(prefix, &part))?;
        let mut w = Writer {
            dir: dir.to_owned(),
            prefix: prefix.to_owned(),
            limits,
            part,
            out: Some(BufWriter::new(file)),
            written: 0,
            redactor: LogRedactor::new(home),
            ring: Arc::new(Mutex::new(VecDeque::with_capacity(RING_LINES))),
            last: None,
            repeats: 0,
            echo,
        };
        for h in header {
            let text = w.redactor.redact(h);
            w.emit(&text);
        }
        Ok(w)
    }

    /// The ring this writer fills.
    pub fn ring(&self) -> Ring {
        Arc::clone(&self.ring)
    }

    /// The file written now.
    pub fn path(&self) -> PathBuf {
        self.dir.join(files::name(&self.prefix, &self.part))
    }

    /// Whether a repeat count waits to be written.
    pub fn repeat_pending(&self) -> bool {
        self.repeats > 0
    }

    /// Takes one line; `dropped` lines were lost before it.
    pub fn line(&mut self, line: &Line, dropped: u64) {
        let same = self
            .last
            .as_ref()
            .is_some_and(|(f, l, t)| *f == line.file && *l == line.line && *t == line.text);
        if same && dropped == 0 {
            self.repeats += 1;
            return;
        }
        self.end_repeat();
        if dropped > 0 {
            self.emit(&format!("({dropped} lines dropped)"));
        }
        let text = self.redactor.redact(&line.text);
        self.emit(&format!(
            "{} {} {} [{}] {}",
            utc_millis(line.time),
            line.level.label(),
            line.area.name(),
            line.thread,
            text
        ));
        self.last = Some((line.file, line.line, line.text.clone()));
    }

    /// Writes a pending repeat count.
    pub fn end_repeat(&mut self) {
        if self.repeats > 0 {
            let n = self.repeats;
            self.repeats = 0;
            let text = if n == 1 {
                "(repeated once)".to_owned()
            } else {
                format!("(repeated {n} times)")
            };
            self.emit(&text);
        }
    }

    /// Writes buffered bytes to the file. Leaves a repeat count pending.
    pub fn flush_file(&mut self) {
        if let Some(out) = &mut self.out
            && out.flush().is_err()
        {
            self.out = None;
        }
    }

    fn emit(&mut self, text: &str) {
        if self.echo {
            rbs_crash::errln!("{text}");
        }
        if let Ok(mut ring) = self.ring.lock() {
            if ring.len() == RING_LINES {
                ring.pop_front();
            }
            ring.push_back(text.to_owned());
        }
        let Some(out) = &mut self.out else {
            return;
        };
        if out
            .write_all(text.as_bytes())
            .and_then(|()| out.write_all(b"\n"))
            .is_err()
        {
            // A full disk or a failed write stops the file, never the app.
            self.out = None;
            return;
        }
        self.written += text.len() as u64 + 1;
        if self.written >= self.limits.file {
            self.rotate();
        }
    }

    fn rotate(&mut self) {
        self.flush_file();
        let next = Part {
            segment: self.part.segment + 1,
            ..self.part.clone()
        };
        self.out = None; // Unlocks the old segment, so pruning may remove it.
        self.written = 0;
        self.part = next;
        self.out = files::create_locked(&self.dir, &files::name(&self.prefix, &self.part))
            .ok()
            .map(BufWriter::new);
        files::prune(&self.dir, &self.prefix, &self.part, &self.limits);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // Tests fail by panicking.
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rbs-log-writer-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn part() -> Part {
        Part {
            time: "20261120T152735Z".to_owned(),
            pid: 42,
            segment: 0,
        }
    }

    fn line(text: &str, at: u32) -> Line {
        Line {
            time: UNIX_EPOCH + Duration::from_millis(1_795_188_455_123),
            level: Level::Info,
            area: Area::Record,
            thread: "rbs-control".to_owned(),
            file: "src/x.rs",
            line: at,
            text: text.to_owned(),
        }
    }

    fn read(w: &mut Writer) -> String {
        w.flush_file();
        std::fs::read_to_string(w.path()).unwrap()
    }

    #[test]
    fn a_line_has_time_level_area_thread_and_redacted_text() {
        let d = dir("format");
        let mut w = Writer::open(
            &d,
            "rbs",
            part(),
            Limits::default(),
            Some("/home/ann".into()),
            &["RBS 0.0.1".into()],
            false,
        )
        .unwrap();
        w.line(&line("saved /home/ann/a.mkv to rtmp://h/app/key", 1), 0);
        assert_eq!(
            read(&mut w),
            "RBS 0.0.1\n2026-11-20T15:27:35.123Z INFO  record [rbs-control] saved ~/a.mkv to rtmp://h/<redacted>\n"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn repeats_collapse_across_flushes() {
        let d = dir("repeat");
        let mut w = Writer::open(&d, "rbs", part(), Limits::default(), None, &[], false).unwrap();
        w.line(&line("slow", 1), 0);
        w.line(&line("slow", 1), 0);
        w.flush_file();
        w.line(&line("slow", 1), 0);
        assert!(w.repeat_pending());
        w.line(&line("slow", 2), 0); // Another call site.
        w.line(&line("other", 2), 0);
        w.line(&line("other", 2), 0);
        w.end_repeat();
        let text = read(&mut w);
        let msgs: Vec<&str> = text
            .lines()
            .map(|l| l.rsplit("] ").next().unwrap_or(l))
            .collect();
        assert_eq!(
            msgs,
            [
                "slow",
                "(repeated 2 times)",
                "slow",
                "other",
                "(repeated once)"
            ]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn dropped_lines_are_counted_before_the_next() {
        let d = dir("dropped");
        let mut w = Writer::open(&d, "rbs", part(), Limits::default(), None, &[], false).unwrap();
        w.line(&line("a", 1), 3);
        assert!(read(&mut w).starts_with("(3 lines dropped)\n"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_full_file_continues_in_a_segment_and_the_ring_keeps_500() {
        let d = dir("rotate");
        let limits = Limits {
            file: 200,
            runs: 10,
            total: 1_000_000,
        };
        let mut w = Writer::open(&d, "rbs", part(), limits, None, &[], false).unwrap();
        for i in 0..600 {
            w.line(&line(&format!("line {i}"), i), 0);
        }
        w.flush_file();
        assert!(
            w.path()
                .ends_with(format!("rbs-20261120T152735Z-42.{}.log", w.part.segment))
        );
        assert!(w.part.segment > 1);
        assert!(d.join("rbs-20261120T152735Z-42.1.log").exists());
        let ring = w.ring();
        let ring = ring.lock().unwrap();
        assert_eq!(ring.len(), RING_LINES);
        assert!(ring.back().unwrap().ends_with("line 599"));
        let _ = std::fs::remove_dir_all(&d);
    }
}
