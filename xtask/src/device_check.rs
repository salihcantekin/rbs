//! `cargo xtask device-check <scenario>`: one command for the checks that only a real
//! Mac, Windows or Linux machine can run (`docs/specs/testing-strategy.md`, "Device
//! checks"; stability plan Faz 4.6).
//!
//! Each scenario is a short list of steps. Every step's output is shown and also written
//! to `<target>/device-check/<scenario>-<unix seconds>/<step>.log`; a summary of each
//! step goes to `summary.txt` beside them, and one result line closes the run.
//!
//! - `lint`: `cargo clippy --workspace --all-targets -D warnings` on this OS, so the code
//!   behind `#[cfg(windows)]` or `#[cfg(target_os = "macos")]` is linted where it builds.
//! - `adapters`: the tests of this OS's adapter crates; hardware tests skip themselves.
//! - `hardware`: the hardware conformance suites, with their `RBS_*_HW` and
//!   `RBS_REQUIRE_*` variables set. Needs a logged-in desktop, a camera and audio
//!   endpoints (`docs/research/windows-checks-sc-pc.md`).
//! - `app`: builds `rbs`, runs it with the UI measure mode (`RBS_MEASURE_UI`, ADR-0120
//!   decision 13) for about `--seconds`, and reports the measure lines, panics, error
//!   lines, new crash reports and how long the quit took after the last measure line.
//!   A run that does not end by the deadline is stopped and reported as a hang.
//! - `all`: `lint`, `adapters`, then `app`.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const USAGE: &str = "\
usage: cargo xtask device-check <lint|adapters|hardware|app|all> [--seconds <n>] [--release]
                               [--fresh-config]
  lint      clippy -D warnings over the workspace, on this OS
  adapters  tests of this OS's adapter crates (hardware tests skip themselves)
  hardware  hardware conformance suites (logged-in desktop, camera, audio endpoints)
  app       build rbs, run it in the UI measure mode for about --seconds (default 60)
            and summarise its log; --release builds the release profile (slow, thin
            LTO), --fresh-config starts from an empty configuration (the first-run page)
  all       lint, adapters, then app
  Logs and summary.txt go to <target>/device-check/<scenario>-<unix seconds>/.";

/// The measure mode's warm-up before its first phase (`rbs-app` `measure_ui`).
const MEASURE_WARM_UP_SECS: u64 = 3;
/// The measure mode's phases: idle, hover, resize, sweep, redraw.
const MEASURE_PHASES: u64 = 5;
/// Time past the planned run before the app counts as hung: startup, the report and a
/// quit that waits on outputs and helpers.
const HANG_GRACE: Duration = Duration::from_secs(60);
/// The most problem lines a summary lists for one step.
const LISTED: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    Mac,
    Linux,
}

impl Os {
    fn current() -> Self {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Mac
        } else {
            Os::Linux
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Lint,
    Adapters,
    Hardware,
    App,
    All,
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    scenario: Scenario,
    seconds: u64,
    release: bool,
    fresh_config: bool,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut iter = args.iter();
    let scenario = match iter.next().map(String::as_str) {
        Some("lint") => Scenario::Lint,
        Some("adapters") => Scenario::Adapters,
        Some("hardware") => Scenario::Hardware,
        Some("app") => Scenario::App,
        Some("all") => Scenario::All,
        Some(other) => return Err(format!("unknown scenario {other}\n{USAGE}")),
        None => return Err(USAGE.to_owned()),
    };
    let mut options = Options {
        scenario,
        seconds: 60,
        release: false,
        fresh_config: false,
    };
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--seconds" => {
                let value = iter.next().ok_or("--seconds needs a number")?;
                options.seconds = value
                    .parse()
                    .ok()
                    .filter(|s| *s >= MEASURE_WARM_UP_SECS + MEASURE_PHASES)
                    .ok_or_else(|| {
                        format!(
                            "--seconds needs a whole number of at least {}",
                            MEASURE_WARM_UP_SECS + MEASURE_PHASES
                        )
                    })?;
            }
            "--release" => options.release = true,
            "--fresh-config" => options.fresh_config = true,
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    Ok(options)
}

/// One `cargo` run of a scenario: its name in the summary, arguments and environment.
#[derive(Debug, PartialEq, Eq)]
struct CargoStep {
    name: String,
    args: Vec<String>,
    env: Vec<(&'static str, &'static str)>,
}

fn step(name: &str, args: &[&str], env: &[(&'static str, &'static str)]) -> CargoStep {
    CargoStep {
        name: name.to_owned(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        env: env.to_vec(),
    }
}

/// The adapter crates that build only on `os` (or only do real work there).
fn adapter_crates(os: Os) -> &'static [&'static str] {
    match os {
        Os::Windows => &[
            "rbs-capture-wgc",
            "rbs-capture-mf",
            "rbs-media-mf",
            "rbs-vcam-mf",
            "rbs-vcam-mf-source",
            "rbs-codec-mf",
            "rbs-codec-nvenc",
            "rbs-codec-amf",
            "rbs-codec-qsv",
            "rbs-audio-io-wasapi",
        ],
        Os::Mac => &[
            "rbs-capture-sck",
            "rbs-capture-avf",
            "rbs-media-avf",
            "rbs-audio-io-coreaudio",
            "rbs-codec-videotoolbox",
            "rbs-codec-audiotoolbox",
        ],
        Os::Linux => &[
            "rbs-capture-pipewire",
            "rbs-audio-io-pipewire",
            "rbs-codec-vaapi",
        ],
    }
}

fn lint_steps() -> Vec<CargoStep> {
    vec![step(
        "clippy",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        &[],
    )]
}

fn adapter_steps(os: Os) -> Vec<CargoStep> {
    let mut args = vec!["test"];
    for name in adapter_crates(os) {
        args.extend(["-p", name]);
    }
    vec![step("adapters", &args, &[])]
}

/// The hardware suites of `os` (`docs/research/windows-checks-sc-pc.md` for Windows).
fn hardware_steps(os: Os) -> Vec<CargoStep> {
    const CAPTURE: (&str, &str) = ("RBS_CAPTURE_HW", "1");
    const AUDIO: (&str, &str) = ("RBS_AUDIO_HW", "1");
    let nocapture = "--nocapture";
    match os {
        Os::Windows => vec![
            step(
                "display",
                &[
                    "test",
                    "-p",
                    "rbs-capture-wgc",
                    "--test",
                    "conformance",
                    "--",
                    "hardware_session",
                    "hardware_thumbnails",
                    nocapture,
                ],
                &[CAPTURE],
            ),
            step(
                "camera",
                &[
                    "test",
                    "-p",
                    "rbs-capture-mf",
                    "--test",
                    "conformance",
                    "--",
                    "hardware_session_passes_conformance",
                    nocapture,
                ],
                &[CAPTURE],
            ),
            step(
                "audio",
                &[
                    "test",
                    "-p",
                    "rbs-audio-io-wasapi",
                    "--test",
                    "conformance",
                    "--",
                    "--ignored",
                    nocapture,
                ],
                &[AUDIO],
            ),
            step(
                "nvenc",
                &[
                    "test",
                    "-p",
                    "rbs-codec-nvenc",
                    "--test",
                    "conformance",
                    "--test",
                    "hardware",
                    "--",
                    nocapture,
                ],
                &[("RBS_REQUIRE_NVENC", "1")],
            ),
            step(
                "amf",
                &["test", "-p", "rbs-codec-amf", "--", nocapture],
                &[("RBS_REQUIRE_AMF", "1")],
            ),
            step(
                "mf-aac",
                &[
                    "test",
                    "-p",
                    "rbs-codec-mf",
                    "--test",
                    "conformance",
                    "--",
                    "media_foundation_passes_the_audio_suite",
                    nocapture,
                ],
                &[],
            ),
        ],
        Os::Mac => vec![
            step(
                "display",
                &[
                    "test",
                    "-p",
                    "rbs-capture-sck",
                    "--test",
                    "conformance",
                    "--",
                    nocapture,
                ],
                &[CAPTURE],
            ),
            step(
                "camera",
                &[
                    "test",
                    "-p",
                    "rbs-capture-avf",
                    "--test",
                    "conformance",
                    "--",
                    nocapture,
                ],
                &[CAPTURE],
            ),
            step(
                "audio",
                &[
                    "test",
                    "-p",
                    "rbs-audio-io-coreaudio",
                    "--test",
                    "conformance",
                    "--",
                    "--ignored",
                    nocapture,
                ],
                &[AUDIO],
            ),
            step(
                "videotoolbox",
                &[
                    "test",
                    "-p",
                    "rbs-codec-videotoolbox",
                    "--test",
                    "conformance",
                    "--",
                    nocapture,
                ],
                &[("RBS_REQUIRE_VIDEOTOOLBOX", "1")],
            ),
        ],
        Os::Linux => vec![
            step(
                "capture",
                &[
                    "test",
                    "-p",
                    "rbs-capture-pipewire",
                    "--test",
                    "conformance",
                    "--",
                    nocapture,
                ],
                &[CAPTURE],
            ),
            step(
                "audio",
                &[
                    "test",
                    "-p",
                    "rbs-audio-io-pipewire",
                    "--test",
                    "conformance",
                    "--",
                    "--ignored",
                    nocapture,
                ],
                &[AUDIO],
            ),
        ],
    }
}

/// The measure mode's seconds per phase for a run of about `seconds`.
fn phase_seconds(seconds: u64) -> u64 {
    (seconds.saturating_sub(MEASURE_WARM_UP_SECS) / MEASURE_PHASES).max(1)
}

/// What one step found, for the summary and the result line.
#[derive(Debug, Default, PartialEq)]
struct StepReport {
    name: String,
    ok: bool,
    /// Short facts for the result line, such as "3 warnings".
    facts: Vec<String>,
    /// Lines worth reading first: lint locations, panics, errors.
    lines: Vec<String>,
}

/// Clippy's and rustc's diagnostics in `log`: each `warning:` or `error:` heading with
/// the location on its `-->` line.
fn summarize_lint(log: &str) -> (usize, usize, Vec<String>) {
    let (mut warnings, mut errors) = (0, 0);
    let mut found = Vec::new();
    let mut heading: Option<String> = None;
    for line in log.lines() {
        let trimmed = line.trim_start();
        let is_total = trimmed.contains("generated") && trimmed.contains("warning")
            || trimmed.starts_with("error: could not compile")
            || trimmed.starts_with("error: aborting")
            || trimmed.starts_with("warning: build failed");
        if is_total {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("warning: ") {
            warnings += 1;
            heading = Some(format!("warning: {rest}"));
        } else if trimmed.starts_with("error: ") || trimmed.starts_with("error[") {
            errors += 1;
            heading = Some(trimmed.to_owned());
        } else if let Some(at) = trimmed.strip_prefix("--> ")
            && let Some(h) = heading.take()
        {
            found.push(format!("{at}: {h}"));
        }
    }
    found.sort();
    found.dedup();
    (warnings, errors, found)
}

/// What the app's log says: measure lines, panics and error lines.
#[derive(Debug, Default, PartialEq)]
struct AppLog {
    measure: Vec<String>,
    panics: Vec<String>,
    errors: Vec<String>,
}

fn summarize_app_log(log: &str) -> AppLog {
    let mut out = AppLog::default();
    for line in log.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("rbs measure:") {
            out.measure.push(trimmed.to_owned());
        } else if trimmed.contains("panicked at") {
            out.panics.push(trimmed.to_owned());
        } else {
            let lower = trimmed.to_ascii_lowercase();
            if ["error", "failed", "fault", "lost"]
                .iter()
                .any(|w| lower.contains(w))
            {
                out.errors.push(trimmed.to_owned());
            }
        }
    }
    out.errors.dedup();
    out
}

/// The configuration directory `rbs` uses: `RBS_CONFIG_DIR`, else the platform's.
/// Mirrors `rbs-remote-settings` `config_dir`.
fn app_config_dir() -> Option<PathBuf> {
    let var = std::env::var_os;
    if let Some(d) = var("RBS_CONFIG_DIR") {
        return Some(PathBuf::from(d));
    }
    match Os::current() {
        Os::Windows => var("APPDATA").map(|d| PathBuf::from(d).join("RBS")),
        Os::Mac => var("HOME").map(|h| {
            PathBuf::from(h)
                .join("Library")
                .join("Application Support")
                .join("RBS")
        }),
        Os::Linux => var("XDG_CONFIG_HOME")
            .filter(|d| !d.is_empty())
            .map(|d| PathBuf::from(d).join("rbs"))
            .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".config").join("rbs"))),
    }
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

pub fn run(root: &Path, args: &[String]) -> Result<ExitCode, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    let options = parse(args)?;
    let os = Os::current();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = format!("{:?}", options.scenario).to_ascii_lowercase();
    let out = target.join("device-check").join(format!("{name}-{stamp}"));
    fs::create_dir_all(&out).map_err(|e| format!("create {}: {e}", out.display()))?;

    let mut steps = Vec::new();
    let (lint, adapters, hardware, app) = match options.scenario {
        Scenario::Lint => (true, false, false, false),
        Scenario::Adapters => (false, true, false, false),
        Scenario::Hardware => (false, false, true, false),
        Scenario::App => (false, false, false, true),
        Scenario::All => (true, true, false, true),
    };
    if lint {
        steps.extend(lint_steps());
    }
    if adapters {
        steps.extend(adapter_steps(os));
    }
    if hardware {
        steps.extend(hardware_steps(os));
    }
    let mut reports = Vec::new();
    for s in &steps {
        reports.push(run_cargo_step(root, &out, s)?);
    }
    if app && reports.iter().all(|r| r.ok) {
        reports.extend(run_app(root, &target, &out, &options)?);
    }
    finish(&out, &name, os, &reports)
}

fn finish(out: &Path, name: &str, os: Os, reports: &[StepReport]) -> Result<ExitCode, String> {
    let all_ok = reports.iter().all(|r| r.ok);
    let parts: Vec<String> = reports
        .iter()
        .map(|r| {
            let state = if r.ok { "ok" } else { "FAILED" };
            if r.facts.is_empty() {
                format!("{} {state}", r.name)
            } else {
                format!("{} {state} ({})", r.name, r.facts.join(", "))
            }
        })
        .collect();
    let line = format!(
        "device-check {name} on {os:?}: {}: {}",
        if all_ok { "passed" } else { "FAILED" },
        parts.join(", ")
    );
    let mut summary = String::new();
    for r in reports {
        if r.lines.is_empty() {
            continue;
        }
        summary.push_str(&format!("[{}]\n", r.name));
        for l in r.lines.iter().take(LISTED) {
            summary.push_str(&format!("  {l}\n"));
        }
        if r.lines.len() > LISTED {
            summary.push_str(&format!(
                "  ... {} more in {}.log\n",
                r.lines.len() - LISTED,
                r.name
            ));
        }
    }
    summary.push_str(&line);
    summary.push('\n');
    let path = out.join("summary.txt");
    fs::write(&path, &summary).map_err(|e| format!("write {}: {e}", path.display()))?;
    eprintln!("\n{summary}logs: {}", out.display());
    Ok(if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn run_cargo_step(root: &Path, out: &Path, s: &CargoStep) -> Result<StepReport, String> {
    let (cargo, mut command) = crate::cargo_command();
    command.current_dir(root).args(&s.args);
    for (k, v) in &s.env {
        command.env(k, v);
    }
    eprintln!("device-check: {} (cargo {})", s.name, s.args.join(" "));
    let started = Instant::now();
    let child = spawn_piped(&mut command).map_err(|e| format!("run {cargo}: {e}"))?;
    let log_path = out.join(format!("{}.log", s.name));
    let (status, log, _) = tee(child, &log_path, None)?;
    let mut report = StepReport {
        name: s.name.clone(),
        ok: status == Some(true),
        ..StepReport::default()
    };
    if s.args.first().is_some_and(|a| a == "clippy") {
        let (warnings, errors, found) = summarize_lint(&log);
        report.facts.push(format!("{warnings} warnings"));
        if errors > 0 {
            report.facts.push(format!("{errors} errors"));
        }
        report.lines = found;
    } else {
        report.lines = log
            .lines()
            .filter(|l| l.contains("FAILED") || l.contains("panicked at"))
            .map(|l| l.trim().to_owned())
            .collect();
    }
    report
        .facts
        .push(format!("{} s", started.elapsed().as_secs()));
    Ok(report)
}

fn spawn_piped(command: &mut Command) -> std::io::Result<Child> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
}

/// Copies the child's stdout and stderr to this process's stderr and to `log_path`,
/// waits for it and returns whether it succeeded (`None` when it was stopped at
/// `deadline`), the whole log and when the last `rbs measure:` line arrived.
fn tee(
    mut child: Child,
    log_path: &Path,
    deadline: Option<Instant>,
) -> Result<(Option<bool>, String, Option<Instant>), String> {
    let (send, lines) = mpsc::channel::<(Instant, String)>();
    let mut readers = Vec::new();
    let pipes: Vec<Box<dyn Read + Send>> = [
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .collect();
    for pipe in pipes {
        let send = send.clone();
        readers.push(thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                if send.send((Instant::now(), line)).is_err() {
                    break;
                }
            }
        }));
    }
    drop(send);
    let mut file =
        fs::File::create(log_path).map_err(|e| format!("create {}: {e}", log_path.display()))?;
    let mut log = String::new();
    let mut last_measure = None;
    let mut stopped = false;
    loop {
        match lines.recv_timeout(Duration::from_millis(200)) {
            Ok((at, line)) => {
                eprintln!("{line}");
                let _ = writeln!(file, "{line}");
                if line.trim_start().starts_with("rbs measure:") {
                    last_measure = Some(at);
                }
                log.push_str(&line);
                log.push('\n');
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if !stopped && deadline.is_some_and(|d| Instant::now() >= d) {
            // A hung app: stop it; its pipes close and the readers end.
            let _ = child.kill();
            stopped = true;
        }
    }
    for r in readers {
        let _ = r.join();
    }
    let status = child.wait().map_err(|e| format!("wait: {e}"))?;
    Ok((
        if stopped {
            None
        } else {
            Some(status.success())
        },
        log,
        last_measure,
    ))
}

fn run_app(
    root: &Path,
    target: &Path,
    out: &Path,
    options: &Options,
) -> Result<Vec<StepReport>, String> {
    let mut build_args = vec!["build", "-p", "rbs-app", "--bin", "rbs"];
    if options.release {
        build_args.push("--release");
    }
    let build = run_cargo_step(root, out, &step("build", &build_args, &[]))?;
    if !build.ok {
        return Ok(vec![build]);
    }
    let mut reports = vec![build];
    let exe = target
        .join(if options.release { "release" } else { "debug" })
        .join(format!("rbs{}", std::env::consts::EXE_SUFFIX));
    let config = if options.fresh_config {
        Some(out.join("config"))
    } else {
        app_config_dir()
    };
    let crash_dir = config.as_ref().map(|c| c.join("crash-reports"));
    let crashes_before = crash_dir.as_deref().map(file_names).unwrap_or_default();

    let phase = phase_seconds(options.seconds);
    let planned = Duration::from_secs(MEASURE_WARM_UP_SECS + MEASURE_PHASES * phase);
    let mut command = Command::new(&exe);
    command
        .current_dir(root)
        .env("RBS_MEASURE_UI", phase.to_string());
    if let Some(c) = config.as_ref().filter(|_| options.fresh_config) {
        command.env("RBS_CONFIG_DIR", c);
    }
    eprintln!(
        "device-check: app ({} for about {} s)",
        exe.display(),
        planned.as_secs()
    );
    let started = Instant::now();
    let child = spawn_piped(&mut command).map_err(|e| format!("run {}: {e}", exe.display()))?;
    let (status, log, last_measure) = tee(
        child,
        &out.join("app.log"),
        Some(started + planned + HANG_GRACE),
    )?;
    let ended = Instant::now();

    let found = summarize_app_log(&log);
    let new_crashes: Vec<String> = crash_dir
        .as_deref()
        .map(file_names)
        .unwrap_or_default()
        .into_iter()
        // `running-<pid>.lock` is the running marker (ADR-0055), not a report.
        .filter(|n| !n.ends_with(".lock") && !crashes_before.contains(n))
        .collect();
    let mut report = StepReport {
        name: "app".to_owned(),
        ..StepReport::default()
    };
    report
        .facts
        .push(format!("{} s", (ended - started).as_secs()));
    match (status, last_measure) {
        (None, _) => report.facts.push("HUNG, stopped".to_owned()),
        (Some(false), _) => report.facts.push("exited with an error".to_owned()),
        (Some(true), None) => report.facts.push("no measure report".to_owned()),
        (Some(true), Some(at)) => report
            .facts
            .push(format!("quit {} ms", (ended - at).as_millis())),
    }
    report.facts.push(format!("{} panics", found.panics.len()));
    report
        .facts
        .push(format!("{} error lines", found.errors.len()));
    if !new_crashes.is_empty() {
        report
            .facts
            .push(format!("{} crash reports", new_crashes.len()));
    }
    if !options.release {
        report.facts.push("dev build".to_owned());
    }
    report.ok = status == Some(true)
        && last_measure.is_some()
        && found.panics.is_empty()
        && new_crashes.is_empty();
    report.lines.extend(found.measure);
    report.lines.extend(found.panics);
    report
        .lines
        .extend(new_crashes.iter().map(|n| format!("crash report: {n}")));
    report.lines.extend(found.errors);
    reports.push(report);
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| (*a).to_owned()).collect()
    }

    #[test]
    fn parses_a_scenario_and_its_options() {
        let o = parse(&args(&["app", "--seconds", "30", "--fresh-config"])).unwrap();
        assert_eq!(o.scenario, Scenario::App);
        assert_eq!(o.seconds, 30);
        assert!(o.fresh_config && !o.release);
        assert_eq!(parse(&args(&["lint"])).unwrap().seconds, 60);
        assert!(parse(&args(&[])).is_err());
        assert!(parse(&args(&["fly"])).is_err());
        assert!(parse(&args(&["app", "--seconds", "3"])).is_err());
        assert!(parse(&args(&["app", "--seconds"])).is_err());
    }

    #[test]
    fn a_minute_is_five_phases_after_the_warm_up() {
        assert_eq!(phase_seconds(60), 11);
        assert_eq!(phase_seconds(8), 1);
        assert_eq!(phase_seconds(0), 1);
    }

    #[test]
    fn every_listed_crate_is_in_the_workspace() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        for os in [Os::Windows, Os::Mac, Os::Linux] {
            let mut names: Vec<String> =
                adapter_crates(os).iter().map(|n| (*n).to_owned()).collect();
            for s in hardware_steps(os) {
                let at = s.args.iter().position(|a| a == "-p").unwrap();
                names.push(s.args[at + 1].clone());
            }
            for name in names {
                assert!(
                    root.join("crates").join(&name).join("Cargo.toml").is_file(),
                    "{name} ({os:?}) is not a workspace crate"
                );
            }
        }
    }

    #[test]
    fn adapter_tests_name_each_crate() {
        let s = &adapter_steps(Os::Mac)[0];
        assert_eq!(s.args[0], "test");
        assert_eq!(
            s.args.iter().filter(|a| *a == "-p").count(),
            adapter_crates(Os::Mac).len()
        );
    }

    #[test]
    fn lint_lists_each_diagnostic_with_its_location() {
        let log = "\
warning: unused variable: `x`
  --> crates/rbs-app/src/thumbnail.rs:109:9
   |
error: this could be a `const fn`
 --> crates/rbs-cli/src/interrupt.rs:89:1
warning: `rbs-app` (lib) generated 1 warning
error: could not compile `rbs-cli` due to 1 previous error
";
        let (w, e, found) = summarize_lint(log);
        assert_eq!((w, e), (1, 1));
        assert_eq!(
            found,
            [
                "crates/rbs-app/src/thumbnail.rs:109:9: warning: unused variable: `x`",
                "crates/rbs-cli/src/interrupt.rs:89:1: error: this could be a `const fn`",
            ]
        );
        assert_eq!(
            summarize_lint("    Finished `dev` profile\n"),
            (0, 0, vec![])
        );
    }

    #[test]
    fn the_app_log_separates_measure_lines_panics_and_errors() {
        let log = "\
rbs: starting
rbs measure: effects=theme scale=2 phase=idle frames=3
thread 'main' panicked at crates/rbs-app/src/main.rs:1:1:
rbs: camera start failed: busy
rbs: camera start failed: busy
";
        let found = summarize_app_log(log);
        assert_eq!(found.measure.len(), 1);
        assert_eq!(found.panics.len(), 1);
        assert_eq!(found.errors, ["rbs: camera start failed: busy"]);
    }
}
