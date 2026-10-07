//! Resident `exiftool -stay_open` writer.
//!
//! ExifTool is the only full IIM+XMP writer (plan section 2, research
//! `presets-metadata-ftp.md` section 2). Spawning one process per write
//! costs a Perl startup each time; instead [`Writer`] spawns
//! `exiftool -stay_open True -@ -` once and sends one arg per stdin line,
//! terminated by `-execute`, reading stdout up to the `{ready}` marker.
//!
//! Access serializes on a mutex: one writer, one in-flight command. A dead
//! process restarts on the next write and the command retries once.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use crate::error::{Error, Result};

/// Marker exiftool prints after each `-execute` block (and once at startup).
const READY: &str = "{ready}";

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
    child: Option<RunningChild>,
}

struct RunningChild {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl ProcessTransport {
    /// Spawn `bin -stay_open True -@ -` and wait for the startup `{ready}`.
    pub fn spawn(bin: &Path) -> Result<Self> {
        let mut transport = Self {
            bin: bin.to_owned(),
            child: None,
        };
        transport.reconnect()?;
        Ok(transport)
    }

    fn running(&mut self) -> Result<&mut RunningChild> {
        if self.child.is_none() {
            self.reconnect()?;
        }
        self.child.as_mut().ok_or_else(|| {
            Error::ExifTool(format!("could not start {}", self.bin.to_string_lossy()))
        })
    }

    fn read_block(running: &mut RunningChild) -> Result<String> {
        let mut out = String::new();
        loop {
            let mut line = String::new();
            let n = running.stdout.read_line(&mut line)?;
            if n == 0 {
                return Err(Error::ExifTool("exiftool closed stdout".into()));
            }
            if line.trim_end() == READY {
                return Ok(out);
            }
            out.push_str(&line);
        }
    }
}

impl Transport for ProcessTransport {
    fn execute(&mut self, args: &[String]) -> Result<String> {
        let running = self.running()?;
        for arg in args {
            running.stdin.write_all(arg.as_bytes())?;
            running.stdin.write_all(b"\n")?;
        }
        running.stdin.write_all(b"-execute\n")?;
        running.stdin.flush()?;
        match Self::read_block(running) {
            Ok(out) => Ok(out),
            Err(e) => {
                // The process is unusable; drop it so the next call restarts.
                let _ = self.child.take().map(|mut r| r.child.kill());
                Err(e)
            }
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
            stdout: BufReader::new(stdout),
        };
        // Synchronize with a no-op round-trip: exiftool prints no marker
        // at startup, only after each `-execute`. Without this the first
        // real command could read stale output — or block forever.
        running.stdin.write_all(b"-ver\n-execute\n")?;
        running.stdin.flush()?;
        Self::read_block(&mut running)?;
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
    pub fn write_tags(&self, target: &Path, tags: &[(&str, &str)]) -> Result<()> {
        let mut args = Vec::with_capacity(tags.len() + 2);
        args.push("-overwrite_original".to_owned());
        for (name, value) in tags {
            args.push(format!("-{name}={value}"));
        }
        args.push(target.to_string_lossy().into_owned());

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

/// Exiftool prints `N image files updated` per successful write block.
fn check_updated(output: &str) -> Result<()> {
    if output
        .lines()
        .any(|l| l.trim().contains("image files updated"))
    {
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

    fn ok_step(want: &[&str]) -> ScriptStep {
        ScriptStep {
            want_args: Some(want.iter().map(|s| s.to_string()).collect()),
            reply: Ok("    1 image files updated\n".into()),
        }
    }

    #[test]
    fn write_tags_sends_overwrite_args_target() {
        let writer = Writer::with_transport(ScriptTransport::new(vec![ok_step(&[
            "-overwrite_original",
            "-XMP-crs:Exposure2012=+0.35",
            "photo.NEF",
        ])]));
        writer
            .write_tags(Path::new("photo.NEF"), &[("XMP-crs:Exposure2012", "+0.35")])
            .unwrap();
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
        struct Counting {
            in_flight: std::sync::atomic::AtomicUsize,
            max: std::sync::atomic::AtomicUsize,
        }
        impl Transport for Counting {
            fn execute(&mut self, _args: &[String]) -> Result<String> {
                let n = self
                    .in_flight
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    + 1;
                self.max.fetch_max(n, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(5));
                self.in_flight
                    .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                Ok("    1 image files updated\n".into())
            }
        }
        let writer = Arc::new(Writer::with_transport(Counting {
            in_flight: std::sync::atomic::AtomicUsize::new(0),
            max: std::sync::atomic::AtomicUsize::new(0),
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
        let _ = writer;
    }
}
