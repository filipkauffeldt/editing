//! Resident `exiftool -stay_open` writer.
//!
//! ExifTool is the only full IIM+XMP writer (plan section 2, research
//! `presets-metadata-ftp.md` section 2). Spawning one process per write
//! costs a Perl startup each time; instead [`Writer`] spawns
//! `exiftool -stay_open True -@ -` once and sends one arg per stdin line,
//! terminated by `-execute`, reading stdout up to the `{ready}` marker.
//!
//! Guards:
//! - Values ride inline only when they are single-line text; anything
//!   containing `\r` or `\n` (a Textarea caption) goes out-of-band via
//!   `-TAG<=tempfile`, because an argfile line break splits the arg and
//!   would desync the `{ready}` protocol.
//! - IIM writes run with `-charset iptc=UTF8`, embedded IIM without it
//!   is Latin-1.
//! - The target is guarded with `--`, so a path starting with `-` stays
//!   a filename.
//! - Reads have a timeout on a watchdog thread; a wedged exiftool is
//!   killed and the transport resets, reconnecting on the next command.
//!
//! Access serializes on a mutex: one writer, one in-flight command. A dead
//! process restarts on the next write and the command retries once.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::error::{Error, Result};

/// Marker exiftool prints after each `-execute` block (and once at startup).
const READY: &str = "{ready}";

/// How long one command block may take before the resident process is
/// considered wedged. Perl startup (the first `-ver` handshake included)
/// sits well under this even cold; a hung disk stall is the real case.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Grace window to get the stdout reader back from the watchdog thread.
/// Past this the thread is left detached and the transport reconnects.
const GIVEBACK: Duration = Duration::from_secs(2);

/// One stay-open command: arg lines over stdin, output up to `{ready}`.
pub trait Transport: Send {
    /// Run one command block; returns the raw stdout before `{ready}`.
    fn execute(&mut self, args: &[String]) -> Result<String>;
    /// Reconnect after a failure. Plain `Ok` when there is nothing to do.
    fn reconnect(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The long-lived child process behind [`Writer::resident`].
pub struct ProcessTransport {
    bin: PathBuf,
    read_timeout: Duration,
    child: Option<RunningChild>,
}

struct RunningChild {
    child: Child,
    stdin: ChildStdin,
    /// Swapped out during a read round-trip so the watchdog thread can own
    /// it; restored after the block finishes (or the timeout kills).
    stdout: Option<BufReader<ChildStdout>>,
}

impl ProcessTransport {
    /// Spawn `bin -stay_open True -@ -` and wait for the startup `{ready}`.
    pub fn spawn(bin: &Path) -> Result<Self> {
        Self::spawn_with_timeout(bin, READ_TIMEOUT)
    }

    /// Spawn with a custom read timeout (tests run a wedged process).
    pub fn spawn_with_timeout(bin: &Path, read_timeout: Duration) -> Result<Self> {
        let mut transport = Self {
            bin: bin.to_owned(),
            read_timeout,
            child: None,
        };
        transport.reconnect()?;
        Ok(transport)
    }

    /// Send the args then read the block up to `{ready}` with a watchdog:
    /// after `timeout` the child is killed and the error reported; the
    /// transport reconnects on the next command.
    ///
    /// The reader moves into a watchdog thread (a plain `read_line` can
    /// block forever on a wedged Perl or a hung disk). It sends the
    /// block's outcome, then hands the reader back through a second
    /// message bounded by [`GIVEBACK`]; if a grandchild holds the pipe
    /// past the grace window, the thread is left detached, `stdout`
    /// stays `None`, and the next command reconnects.
    fn round_trip(
        running: &mut RunningChild,
        args: &[String],
        timeout: Duration,
    ) -> Result<String> {
        for arg in args {
            running.stdin.write_all(arg.as_bytes())?;
            running.stdin.write_all(b"\n")?;
        }
        running.stdin.write_all(b"-execute\n")?;
        running.stdin.flush()?;

        let mut stdout = running
            .stdout
            .take()
            .ok_or_else(|| Error::ExifTool("no stdout pipe from the resident exiftool".into()))?;
        type Message = std::result::Result<String, String>;
        type Reader = BufReader<ChildStdout>;
        let (outcome_tx, outcome_rx) = std::sync::mpsc::channel::<Message>();
        let (reader_tx, reader_rx) = std::sync::mpsc::channel::<Option<Reader>>();
        let _watchdog = std::thread::spawn(move || {
            let mut out = String::new();
            loop {
                let mut line = String::new();
                match stdout.read_line(&mut line) {
                    Ok(0) => {
                        let _ = outcome_tx.send(Err("exiftool closed stdout".into()));
                        break;
                    }
                    Ok(_) => {
                        if line.trim_end() == READY {
                            let _ = outcome_tx.send(Ok(out));
                            break;
                        }
                        out.push_str(&line);
                    }
                    Err(e) => {
                        let _ = outcome_tx.send(Err(format!("read: {e}")));
                        break;
                    }
                }
            }
            let _ = reader_tx.send(Some(stdout));
        });

        let outcome: std::result::Result<String, String> = match outcome_rx.recv_timeout(timeout) {
            Ok(outcome) => outcome,
            Err(_) => {
                let _ = running.child.kill();
                let _ = running.child.wait();
                Err(format!(
                    "timed out after {}s waiting for the exiftool block",
                    timeout.as_secs()
                ))
            }
        };
        running.stdout = reader_rx.recv_timeout(GIVEBACK).unwrap_or(None);
        outcome.map_err(Error::ExifTool)
    }
}

impl Transport for ProcessTransport {
    fn execute(&mut self, args: &[String]) -> Result<String> {
        if self.child.is_none() {
            self.reconnect()?;
        }
        match self.child.as_mut() {
            Some(running) => Self::round_trip(running, args, self.read_timeout),
            None => Err(Error::ExifTool(
                "exiftool child vanished between reconnect and write".into(),
            )),
        }
    }

    fn reconnect(&mut self) -> Result<()> {
        if let Some(mut running) = self.child.take() {
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
        let mut child = Command::new(&self.bin)
            .args(["-stay_open", "True", "-@", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| {
                Error::Unavailable(format!(
                    "{}: {e} (install libimage-exiftool-perl)",
                    self.bin.to_string_lossy()
                ))
            })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            Error::ExifTool(format!("no stdin from {}", self.bin.to_string_lossy()))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            Error::ExifTool(format!("no stdout from {}", self.bin.to_string_lossy()))
        })?;
        let mut running = RunningChild {
            child,
            stdin,
            stdout: Some(BufReader::new(stdout)),
        };
        // Synchronize with a no-op round-trip: exiftool prints no marker
        // at startup, only after each `-execute`. Without this the first
        // real command could read stale output — or block forever.
        // `round_trip` appends the `-execute` itself.
        Self::round_trip(&mut running, &["-ver".to_owned()], self.read_timeout)?;
        self.child = Some(running);
        Ok(())
    }
}

impl Drop for ProcessTransport {
    fn drop(&mut self) {
        if let Some(mut running) = self.child.take() {
            // Best-effort graceful close; never block the dropping thread.
            let _ = running.stdin.write_all(b"-stay_open\nFalse\n-execute\n");
            let _ = running.stdin.flush();
            let _ = running.child.wait_timeout_or_kill(Duration::from_secs(2));
        }
    }
}

/// Scripted transport for unit tests: asserts the arg sequence, replays
/// canned outputs.
#[cfg(any(test, feature = "test-transport"))]
pub struct ScriptTransport {
    script: std::collections::VecDeque<ScriptStep>,
}

/// One expected command and its canned response.
#[cfg(any(test, feature = "test-transport"))]
pub struct ScriptStep {
    /// Expected args; `None` skips the assertion for this step.
    pub want_args: Option<Vec<String>>,
    /// Output to replay, or error text for a transport failure.
    pub reply: std::result::Result<String, String>,
}

#[cfg(any(test, feature = "test-transport"))]
impl ScriptTransport {
    /// Build from `(want_args, reply)` steps.
    pub fn new(steps: Vec<ScriptStep>) -> Self {
        Self {
            script: steps.into(),
        }
    }

    /// Assert every scripted step ran.
    pub fn assert_empty(&self) {
        assert!(
            self.script.is_empty(),
            "{} scripted exiftool steps never ran",
            self.script.len()
        );
    }
}

#[cfg(any(test, feature = "test-transport"))]
impl Transport for ScriptTransport {
    fn execute(&mut self, args: &[String]) -> Result<String> {
        let step = self
            .script
            .pop_front()
            .expect("exiftool writer ran an unscripted command");
        if let Some(want) = step.want_args {
            assert_eq!(args, want.as_slice(), "exiftool arg mismatch");
        }
        step.reply.map_err(Error::ExifTool)
    }
}

/// Serialized tag writer over any [`Transport`].
pub struct Writer {
    transport: Mutex<Box<dyn Transport>>,
}

impl Writer {
    /// Writer over a resident `exiftool` process at `bin`.
    pub fn resident(bin: &Path) -> Result<Self> {
        Ok(Self {
            transport: Mutex::new(Box::new(ProcessTransport::spawn(bin)?)),
        })
    }

    /// Writer over an injected transport (tests).
    pub fn with_transport(transport: impl Transport + 'static) -> Self {
        Self {
            transport: Mutex::new(Box::new(transport)),
        }
    }

    /// True when `bin -ver` runs. Gate integration tests on this.
    pub fn is_available(bin: &str) -> bool {
        Command::new(bin)
            .arg("-ver")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Find an exiftool binary: `$EXIFTOOL`, then `PATH`.
    pub fn find_binary() -> Option<PathBuf> {
        if let Some(bin) = std::env::var_os("EXIFTOOL") {
            let bin = PathBuf::from(bin);
            if Self::is_available(&bin.to_string_lossy()) {
                return Some(bin);
            }
        }
        Self::is_available("exiftool").then(|| PathBuf::from("exiftool"))
    }

    /// Set `tags` (`NAME=value`, exiftool `-TAG=value` form) on `target`.
    /// Restarts a dead process once and retries the command once.
    ///
    /// Multiline values (a Textarea caption) ride out-of-band through
    /// `-TAG<=tempfile`: an argfile line break would split the arg and
    /// desync the `{ready}` protocol. Temp files are removed when the
    /// command round-trip ends, success or not.
    ///
    /// No `--` target guard is possible: exiftool's argfile parser
    /// ignores `-execute` after `--` (end-of-options swallows the
    /// command boundary and the stream then waits forever), verified
    /// against 13.59. Instead a target that could parse as an option is
    /// rejected before send, and paths must be UTF-8 text (argfile
    /// lines are text; non-UTF-8 paths are rejected, not mangled).
    pub fn write_tags(&self, target: &Path, tags: &[(&str, &str)]) -> Result<()> {
        let target_text = target
            .to_str()
            .and_then(|t| (!t.contains('\n') && !t.contains('\r')).then(|| t.to_owned()));
        let Some(target_text) = target_text else {
            return Err(Error::InvalidPath(format!(
                "{target:?} (not UTF-8 or contains a line break)"
            )));
        };
        if target_text.starts_with('-') {
            return Err(Error::InvalidPath(format!(
                "{target_text:?} would parse as an exiftool option"
            )));
        }

        // Temp files clean up on drop, even when a mid-command failure
        // unwinds before the explicit cleanup below.
        let mut temp = TempValueFiles::new();
        let mut args = Vec::with_capacity(tags.len() + 5);
        args.push("-charset".to_owned());
        args.push("iptc=UTF8".to_owned());
        args.push("-overwrite_original".to_owned());
        for (name, value) in tags {
            if value.contains('\n') || value.contains('\r') {
                let file = temp.create(value)?;
                args.push(format!("-{name}<={}", file.display()));
            } else {
                args.push(format!("-{name}={value}"));
            }
        }
        args.push(target_text);

        let mut transport = self
            .transport
            .lock()
            .map_err(|_| Error::ExifTool("writer lock poisoned".into()))?;
        let output = match transport.execute(&args) {
            Ok(out) => out,
            Err(first) => {
                transport.reconnect()?;
                transport
                    .execute(&args)
                    .map_err(|retry| Error::ExifTool(format!("after restart ({first}): {retry}")))?
            }
        };
        check_updated(&output)
    }
}

/// Owns the value files created for `-TAG<=` assignments during one
/// `write_tags` command; Drop removes them (best-effort).
struct TempValueFiles {
    prefix: String,
    next: u64,
    paths: Vec<PathBuf>,
}

impl TempValueFiles {
    fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        Self {
            prefix: format!(
                "editing-exif-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ),
            next: 0,
            paths: Vec::new(),
        }
    }

    fn create(&mut self, content: &str) -> Result<PathBuf> {
        let path = std::env::temp_dir().join(format!("{}.{}.txt", self.prefix, self.next));
        self.next += 1;
        std::fs::write(&path, content.as_bytes())?;
        self.paths.push(path.clone());
        Ok(path)
    }

    fn cleanup(&mut self) {
        for path in self.paths.drain(..) {
            std::fs::remove_file(&path).ok();
        }
    }
}

impl Drop for TempValueFiles {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// Exiftool prints `N image files updated` per write block. The count
/// matters: a failure summary reads `0 image files updated`, which must
/// not report success. An idempotent re-run prints `0 image files
/// updated / N image files unchanged` — that is a success too, so a
/// crash-resumed metadata job does not mark already-written photos red.
fn check_updated(output: &str) -> Result<()> {
    if updated_count(output) >= 1 || unchanged_count(output) >= 1 {
        return Ok(());
    }
    Err(Error::ExifTool(
        output
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("; "),
    ))
}

fn summary_count(output: &str, marker: &str) -> usize {
    output
        .lines()
        .filter(|l| l.contains(marker))
        .filter_map(|l| l.split_whitespace().next())
        .filter_map(|n| n.parse::<usize>().ok())
        .sum()
}

fn updated_count(output: &str) -> usize {
    summary_count(output, "image files updated")
}

fn unchanged_count(output: &str) -> usize {
    summary_count(output, "image files unchanged")
}

/// POSIX `wait_timeout_or_kill`: wait up to `timeout`, then kill.
trait WaitTimeoutOrKill {
    fn wait_timeout_or_kill(&mut self, timeout: Duration) -> std::io::Result<()>;
}

impl WaitTimeoutOrKill for Child {
    fn wait_timeout_or_kill(&mut self, timeout: Duration) -> std::io::Result<()> {
        use std::time::Instant;
        let start = Instant::now();
        loop {
            match self.try_wait()? {
                Some(_) => return Ok(()),
                None if start.elapsed() >= timeout => {
                    self.kill()?;
                    self.wait()?;
                    return Ok(());
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ok_step(want: &[&str]) -> ScriptStep {
        ScriptStep {
            want_args: Some(want.iter().map(|s| s.to_string()).collect()),
            reply: Ok("    1 image files updated\n".into()),
        }
    }

    #[test]
    fn write_tags_sends_charset_overwrite_and_target_guard() {
        let writer = Writer::with_transport(ScriptTransport::new(vec![ok_step(&[
            "-charset",
            "iptc=UTF8",
            "-overwrite_original",
            "-XMP-crs:Exposure2012=+0.35",
            "photo.NEF",
        ])]));
        writer
            .write_tags(Path::new("photo.NEF"), &[("XMP-crs:Exposure2012", "+0.35")])
            .unwrap();
    }

    #[test]
    fn multiline_values_ride_out_of_band_through_a_value_file() {
        // A newline inside one arg would split the argfile line; the
        // writer must emit `-TAG<=tmp` instead, one arg per line, with
        // the `--` guard before the target, and delete the value file
        // after the command.
        let captured = Arc::new(Mutex::new(Vec::<String>::new()));
        struct Cap(Arc<Mutex<Vec<String>>>);
        impl Transport for Cap {
            fn execute(&mut self, args: &[String]) -> Result<String> {
                *self.0.lock().unwrap() = args.to_vec();
                Ok("    1 image files updated\n".into())
            }
        }
        Writer::with_transport(Cap(Arc::clone(&captured)))
            .write_tags(
                Path::new("p.NEF"),
                &[("IPTC:Caption-Abstract", "line one\nline two")],
            )
            .unwrap();
        let args = captured.lock().unwrap().clone();
        assert_eq!(
            &args[0..3],
            &["-charset", "iptc=UTF8", "-overwrite_original"]
        );
        let tag_arg = &args[3];
        assert!(
            tag_arg.starts_with("-IPTC:Caption-Abstract<="),
            "multiline value must ride a value file, got {tag_arg:?}"
        );
        assert_eq!(args[4], "p.NEF");
        let file = tag_arg
            .strip_prefix("-IPTC:Caption-Abstract<=")
            .expect("value file path");
        assert!(
            !Path::new(file).exists(),
            "value file {file} was not cleaned up"
        );
    }

    #[test]
    fn no_arg_ever_contains_a_line_break() {
        struct Guard;
        impl Transport for Guard {
            fn execute(&mut self, args: &[String]) -> Result<String> {
                for arg in args {
                    assert!(!arg.contains('\n'), "arg with line break: {arg:?}");
                    assert!(!arg.contains('\r'), "arg with return: {arg:?}");
                }
                Ok("    1 image files updated\n".into())
            }
        }
        let writer = Writer::with_transport(Guard);
        writer
            .write_tags(
                Path::new("photo.NEF"),
                &[
                    ("XMP-dc:Description", "one\ntwo\rthree"),
                    ("IPTC:Keywords", "plain"),
                ],
            )
            .unwrap();
    }

    #[test]
    fn non_utf8_paths_are_rejected_not_mangled() {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let bad = std::ffi::OsStr::from_bytes(b"photo-\xff.NEF");
            let writer = Writer::with_transport(ScriptTransport::new(vec![]));
            let err = writer
                .write_tags(Path::new(bad), &[("XMP-dc:Description", "x")])
                .unwrap_err();
            assert!(err.to_string().contains("not UTF-8"), "{err}");
        }
    }

    #[test]
    fn dash_leading_targets_are_rejected() {
        let writer = Writer::with_transport(ScriptTransport::new(vec![]));
        let err = writer
            .write_tags(Path::new("-weird.NEF"), &[("XMP-dc:Description", "x")])
            .unwrap_err();
        assert!(err.to_string().contains("an exiftool option"), "{err}");
    }

    #[test]
    fn unchanged_files_report_success() {
        // Exiftool prints `0 image files updated / 1 image files
        // unchanged` when a metadata_write job is re-run after a crash:
        // the tags are already on the photo, which must not go red.
        let writer = Writer::with_transport(ScriptTransport::new(vec![ScriptStep {
            want_args: None,
            reply: Ok("    0 image files updated\n    1 image files unchanged\n".into()),
        }]));
        writer
            .write_tags(Path::new("photo.NEF"), &[("XMP-dc:Description", "x")])
            .unwrap();
    }

    #[test]
    fn nothing_updated_and_nothing_unchanged_is_an_error() {
        let writer = Writer::with_transport(ScriptTransport::new(vec![ScriptStep {
            want_args: None,
            reply: Ok(
                "    0 image files updated\n    1 files weren't updated due to errors\n".into(),
            ),
        }]));
        let err = writer
            .write_tags(Path::new("photo.NEF"), &[("XMP-dc:Description", "x")])
            .unwrap_err();
        assert!(err.to_string().contains("0 image files updated"), "{err}");
    }

    #[test]
    fn updated_count_sums_only_success_lines() {
        assert_eq!(updated_count("    1 image files updated\n"), 1);
        assert_eq!(updated_count("    0 image files updated\n"), 0);
        assert_eq!(updated_count("Error: Nothing to write\n"), 0);
        assert_eq!(updated_count(""), 0);
        assert_eq!(unchanged_count("    2 image files unchanged\n"), 2);
        assert_eq!(unchanged_count("    1 image files updated\n"), 0);
    }

    #[test]
    fn write_tags_reports_exiftool_errors() {
        let writer = Writer::with_transport(ScriptTransport::new(vec![
            ScriptStep {
                want_args: None,
                reply: Ok("Error: Nothing to write\n".into()),
            },
            // Retry after reconnect also fails: the error surfaces.
            ScriptStep {
                want_args: None,
                reply: Ok("Error: Nothing to write\n".into()),
            },
        ]));
        let err = writer
            .write_tags(Path::new("photo.NEF"), &[("XMP-crs:Exposure2012", "+0.35")])
            .unwrap_err();
        assert!(err.to_string().contains("Nothing to write"), "{err}");
    }

    #[test]
    fn write_tags_rejects_zero_updated_count() {
        let writer = Writer::with_transport(ScriptTransport::new(vec![ScriptStep {
            want_args: None,
            reply: Ok(
                "    0 image files updated\n    1 files weren't updated due to errors\n".into(),
            ),
        }]));
        let err = writer
            .write_tags(Path::new("photo.NEF"), &[("XMP-crs:Exposure2012", "+0.35")])
            .unwrap_err();
        assert!(err.to_string().contains("0 image files updated"), "{err}");
    }

    #[test]
    fn write_tags_restarts_dead_process_and_retries_once() {
        struct Flaky {
            calls: usize,
        }
        impl Transport for Flaky {
            fn execute(&mut self, _args: &[String]) -> Result<String> {
                self.calls += 1;
                if self.calls == 1 {
                    return Err(Error::ExifTool("exiftool closed stdout".into()));
                }
                Ok("    1 image files updated\n".into())
            }
        }
        let writer = Writer::with_transport(Flaky { calls: 0 });
        writer
            .write_tags(Path::new("photo.NEF"), &[("XMP-crs:Exposure2012", "+0.35")])
            .unwrap();
    }

    #[test]
    fn concurrent_writes_serialize_on_one_transport() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Counting {
            in_flight: Arc<AtomicUsize>,
            max: Arc<AtomicUsize>,
        }
        impl Transport for Counting {
            fn execute(&mut self, _args: &[String]) -> Result<String> {
                let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                self.max.fetch_max(n, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(5));
                self.in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok("    1 image files updated\n".into())
            }
        }
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let writer = Arc::new(Writer::with_transport(Counting {
            in_flight: Arc::clone(&in_flight),
            max: Arc::clone(&max),
        }));
        std::thread::scope(|s| {
            for _ in 0..8 {
                let writer = Arc::clone(&writer);
                s.spawn(move || {
                    writer
                        .write_tags(Path::new("p.NEF"), &[("XMP-crs:Exposure2012", "0")])
                        .unwrap();
                });
            }
        });
        assert_eq!(
            max.load(Ordering::SeqCst),
            1,
            "writes overlapped on one transport"
        );
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn wedged_process_times_out_and_reconnects() {
        // A bin whose `exiftool` never answers wedges the read; the
        // watchdog kills it and reconnect() must fail with the timeout
        // diagnostic instead of blocking forever.
        let dir = tempfile::TempDir::new().unwrap();
        let bin = dir.path().join("wedged-exiftool");
        // `exec` so the shell pid is the sleeper itself: the kill must
        // reach the process holding the stdout pipe, or EOF never comes.
        std::fs::write(&bin, "#!/bin/sh\nexec sleep 300\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let start = std::time::Instant::now();
        let err = ProcessTransport::spawn_with_timeout(&bin, Duration::from_millis(300))
            .map(|_| ())
            .expect_err("spawn must fail on the wedged process");
        assert!(
            err.to_string().contains("timed out"),
            "expect a timeout diagnostic, got {err}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "timeout did not fire"
        );
    }
}
