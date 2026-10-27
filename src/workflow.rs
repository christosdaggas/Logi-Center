//! Bounded, direct-exec desktop workflows. These are trusted local configuration,
//! not a sandbox: profile import must leave actions disabled until reviewed.
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::profiles::KeyChord;

pub const MAX_STEPS: usize = 64;
pub const MAX_DURATION_MS: u64 = 30_000;
pub const QUEUE_CAPACITY: usize = 16;
const MAX_TEXT_BYTES: usize = 32_768;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Workflow {
    pub name: String,
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum Step {
    Shortcut(KeyChord),
    Delay(u32),
    /// Direct executable only; never interpreted by a shell. A launch is waited
    /// for, with a bounded timeout. Its entire process group is cleaned up.
    Launch {
        program: String,
        args: Vec<String>,
        timeout_ms: u32,
    },
    Media(MediaAction),
    /// Hand a document/web address to the desktop's default application. Unlike
    /// Launch, this intentionally lets the desktop own the application's life.
    OpenUri(String),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum MediaAction {
    PlayPause,
    Next,
    Previous,
    Mute,
    VolumeUp,
    VolumeDown,
}

impl MediaAction {
    pub fn chord(self) -> KeyChord {
        let key = match self {
            Self::PlayPause => 164,
            Self::Next => 163,
            Self::Previous => 165,
            Self::Mute => 113,
            Self::VolumeUp => 115,
            Self::VolumeDown => 114,
        };
        KeyChord {
            keys: vec![key],
            label: format!("{self:?}"),
        }
    }
}

impl Workflow {
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty()
            || self.name.len() > 160
            || self.name.chars().any(char::is_control)
            || self.steps.is_empty()
            || self.steps.len() > MAX_STEPS
        {
            bail!("A workflow needs a name (1–160 bytes) and 1–64 steps");
        }
        let mut duration = 0u64;
        for step in &self.steps {
            duration = duration.checked_add(match step {
                Step::Shortcut(chord) => { chord.validate()?; 20 },
                Step::Media(_) => 20,
                Step::OpenUri(uri) => { validate_uri(uri)?; 100 },
                Step::Delay(ms) => {
                    if *ms > 10_000 { bail!("Each delay is limited to 10 seconds"); }
                    u64::from(*ms)
                },
                Step::Launch { program, args, timeout_ms } => {
                    let path = Path::new(program);
                    let basename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                    // No implicit PATH lookup or shell escape hatches. This is
                    // defense in depth, not a claim to sandbox a trusted binary.
                    if !path.is_absolute() || program.len() > 4096
                        || program.chars().any(char::is_control)
                        || basename.is_empty()
                        || matches!(basename, "sh" | "bash" | "dash" | "zsh" | "fish" | "ksh" | "csh" | "tcsh" | "env" | "busybox" | "sudo" | "pkexec")
                        || args.len() > 32 || args.iter().any(|s| s.len() > 2048 || s.contains('\0'))
                        || !(10..=10_000).contains(timeout_ms) {
                        bail!("Launch requires an absolute, non-shell executable, ≤32 bounded arguments and a 10–10000 ms timeout");
                    }
                    u64::from(*timeout_ms)
                },
            }).context("Workflow duration overflow")?;
        }
        if duration > MAX_DURATION_MS || serde_json::to_vec(self)?.len() > MAX_TEXT_BYTES {
            bail!("Workflow exceeds 30 seconds or 32 KiB");
        }
        Ok(())
    }

    pub fn needs_keyboard(&self) -> bool {
        self.steps
            .iter()
            .any(|s| matches!(s, Step::Shortcut(_) | Step::Media(_)))
    }
}

/// Non-launching presets: selecting one only changes the editor draft.
pub fn templates() -> Vec<Workflow> {
    let key = |keys: Vec<u16>, label: &str| {
        Step::Shortcut(KeyChord {
            keys,
            label: label.into(),
        })
    };
    vec![
        Workflow {
            name: "Copy then paste".into(),
            steps: vec![
                key(vec![29, 46], "Ctrl+C"),
                Step::Delay(150),
                key(vec![29, 47], "Ctrl+V"),
            ],
        },
        Workflow {
            name: "Screenshot region".into(),
            steps: vec![key(vec![42, 99], "Shift+Print")],
        },
        Workflow {
            name: "Media play/pause".into(),
            steps: vec![Step::Media(MediaAction::PlayPause)],
        },
        Workflow {
            name: "Next workspace".into(),
            steps: vec![key(vec![29, 56, 106], "Ctrl+Alt+Right")],
        },
    ]
}

#[derive(Clone)]
pub struct Cancellation {
    epoch: Arc<AtomicU64>,
    generation: u64,
    stop: Arc<AtomicBool>,
}

impl Cancellation {
    pub fn check(&self) -> Result<()> {
        if self.stop.load(Ordering::Acquire)
            || self.epoch.load(Ordering::Acquire) != self.generation
        {
            bail!("Action cancelled");
        }
        Ok(())
    }

    pub fn wait(&self, duration: Duration) -> Result<()> {
        let deadline = Instant::now() + duration;
        loop {
            self.check()?;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            std::thread::sleep(left.min(Duration::from_millis(10)));
        }
    }
}

pub trait Host {
    fn shortcut(&mut self, chord: &KeyChord) -> Result<()>;
    fn launch(
        &mut self,
        program: &str,
        args: &[String],
        timeout: Duration,
        cancel: &Cancellation,
    ) -> Result<()>;
    fn open_uri(&mut self, _uri: &str) -> Result<()> {
        bail!("This workflow host does not support opening locations")
    }
}

fn validate_uri(uri: &str) -> Result<()> {
    if uri.len() > 4096 || uri.chars().any(char::is_control) {
        bail!("Location exceeds its size limit or contains control characters");
    }
    let parsed = glib::Uri::parse(uri, glib::UriFlags::NONE).context("Invalid location URI")?;
    match parsed.scheme().as_str() {
        "http" | "https" if parsed.host().is_some_and(|h| !h.is_empty()) => Ok(()),
        "file"
            if parsed
                .host()
                .is_none_or(|h| h.is_empty() || h == "localhost")
                && parsed.path().starts_with('/') =>
        {
            Ok(())
        }
        _ => bail!("Only http, https and absolute local file URIs are supported"),
    }
}

pub struct SystemHost {
    keyboard: Option<crate::input::Keyboard>,
}

impl SystemHost {
    pub fn new(keyboard: Option<crate::input::Keyboard>) -> Self {
        Self { keyboard }
    }
}

impl Host for SystemHost {
    fn open_uri(&mut self, uri: &str) -> Result<()> {
        validate_uri(uri)?;
        gtk4::gio::AppInfo::launch_default_for_uri(uri, None::<&gtk4::gio::AppLaunchContext>)
            .context("The desktop could not open this location")
    }
    fn shortcut(&mut self, chord: &KeyChord) -> Result<()> {
        if self.keyboard.is_none() {
            self.keyboard = Some(crate::input::Keyboard::open()?);
        }
        let result = self.keyboard.as_mut().unwrap().send(chord);
        if result.is_err() {
            self.keyboard.take();
        }
        result
    }

    fn launch(
        &mut self,
        program: &str,
        args: &[String],
        timeout: Duration,
        cancel: &Cancellation,
    ) -> Result<()> {
        use std::os::unix::process::CommandExt;
        cancel.check()?;
        let mut command = Command::new(program);
        // No captured output, inherited terminal, shell or unbounded buffers.
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);
        let parent = unsafe { libc::getpid() };
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(std::io::Error::other(
                        "Workflow parent exited before launch",
                    ));
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .with_context(|| format!("Could not launch {program}"))?;
        let mut guard = ChildGuard {
            child,
            reaped: false,
        };
        let deadline = Instant::now() + timeout;
        loop {
            cancel.check()?;
            if guard.exited()? {
                let status = guard.finish()?;
                if !status.success() {
                    bail!("Program exited with {status}");
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("Program exceeded its timeout");
            }
            cancel.wait(Duration::from_millis(10))?;
        }
    }
}

struct ChildGuard {
    child: Child,
    reaped: bool,
}
impl ChildGuard {
    fn exited(&mut self) -> Result<bool> {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                self.reaped = true;
            }
            return Err(error.into());
        }
        Ok(unsafe { info.si_pid() } != 0)
    }
    fn finish(&mut self) -> Result<std::process::ExitStatus> {
        if !self.reaped {
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
        }
        let status = self.child.wait()?;
        self.reaped = true;
        Ok(status)
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        // A separate process group prevents killing our own application. Kill
        // descendants even if the direct child already exited, then reap it.
        if !self.reaped {
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn run(workflow: &Workflow, host: &mut impl Host, cancel: &Cancellation) -> Result<()> {
    workflow.validate()?;
    let deadline = Instant::now() + Duration::from_millis(MAX_DURATION_MS);
    for step in &workflow.steps {
        cancel.check()?;
        if Instant::now() >= deadline {
            bail!("Workflow exceeded its runtime limit");
        }
        match step {
            Step::Shortcut(chord) => host.shortcut(chord)?,
            Step::Delay(ms) => {
                let requested = Duration::from_millis(u64::from(*ms));
                let left = deadline.saturating_duration_since(Instant::now());
                cancel.wait(requested.min(left))?;
                if requested > left {
                    bail!("Workflow exceeded its runtime limit");
                }
            }
            Step::Launch {
                program,
                args,
                timeout_ms,
            } => host.launch(
                program,
                args,
                Duration::from_millis(u64::from(*timeout_ms))
                    .min(deadline.saturating_duration_since(Instant::now())),
                cancel,
            )?,
            Step::Media(action) => host.shortcut(&action.chord())?,
            Step::OpenUri(uri) => host.open_uri(uri)?,
        }
    }
    if Instant::now() > deadline {
        bail!("Workflow exceeded its runtime limit");
    }
    cancel.check()
}

struct Job<T> {
    value: T,
    generation: u64,
}

pub struct Submitter<T> {
    tx: mpsc::SyncSender<Job<T>>,
    epoch: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
}
impl<T> Clone for Submitter<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            epoch: self.epoch.clone(),
            stop: self.stop.clone(),
        }
    }
}
impl<T> Submitter<T> {
    pub fn generation(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }
    pub fn try_submit(&self, value: T) -> Result<()> {
        self.try_submit_generation(value, self.generation())
    }
    pub fn try_submit_generation(&self, value: T, generation: u64) -> Result<()> {
        if self.is_stopped() {
            bail!("Action worker stopped");
        }
        self.tx
            .try_send(Job { value, generation })
            .map_err(|e| anyhow::anyhow!("Action queue unavailable: {e}"))
    }
}

/// A serial, bounded worker. Cancelling invalidates active AND queued work.
/// Drop cancels, closes the queue, and joins; delays/launches check every 10 ms.
pub struct Worker<T> {
    submit: Option<Submitter<T>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
struct MarkStopped(Arc<AtomicBool>);
impl Drop for MarkStopped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
impl<T: Send + 'static> Worker<T> {
    pub fn spawn(
        epoch: Arc<AtomicU64>,
        mut handle: impl FnMut(T, &Cancellation) + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::sync_channel::<Job<T>>(QUEUE_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker_epoch = epoch.clone();
        let thread = std::thread::spawn(move || {
            let _mark_stopped = MarkStopped(worker_stop.clone());
            while !worker_stop.load(Ordering::Acquire) {
                match rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(job) => {
                        let cancel = Cancellation {
                            epoch: worker_epoch.clone(),
                            generation: job.generation,
                            stop: worker_stop.clone(),
                        };
                        if cancel.check().is_ok() {
                            handle(job.value, &cancel);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self {
            submit: Some(Submitter {
                tx,
                epoch,
                stop: stop.clone(),
            }),
            stop,
            thread: Some(thread),
        }
    }
    pub fn submitter(&self) -> Submitter<T> {
        self.submit.as_ref().unwrap().clone()
    }
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
}
impl<T> Drop for Worker<T> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.submit.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    #[derive(Default)]
    struct FakeHost {
        shortcuts: usize,
        launches: usize,
        locations: usize,
    }
    impl Host for FakeHost {
        fn open_uri(&mut self, _: &str) -> Result<()> {
            self.locations += 1;
            Ok(())
        }
        fn shortcut(&mut self, _: &KeyChord) -> Result<()> {
            self.shortcuts += 1;
            Ok(())
        }
        fn launch(&mut self, _: &str, _: &[String], _: Duration, _: &Cancellation) -> Result<()> {
            self.launches += 1;
            Ok(())
        }
    }
    fn token() -> Cancellation {
        Cancellation {
            epoch: Arc::new(AtomicU64::new(0)),
            generation: 0,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
    #[test]
    fn limits_templates_and_direct_launch_validation() {
        for template in templates() {
            template.validate().unwrap();
            assert!(template.needs_keyboard());
        }
        let mut flow = Workflow {
            name: "fixture".into(),
            steps: vec![Step::Delay(10_001)],
        };
        assert!(flow.validate().is_err());
        flow.steps = vec![Step::Delay(1); MAX_STEPS + 1];
        assert!(flow.validate().is_err());
        flow.steps = vec![Step::Delay(10_000); 4];
        assert!(flow.validate().is_err());
        for program in [
            "echo",
            "/bin/sh",
            "/usr/bin/bash",
            "/usr/bin/env",
            "/bin/busybox",
            "/usr/bin/sudo",
        ] {
            flow.steps = vec![Step::Launch {
                program: program.into(),
                args: vec!["-c".into(), "untrusted".into()],
                timeout_ms: 100,
            }];
            assert!(flow.validate().is_err());
        }
        flow.steps = vec![Step::Launch {
            program: "/approved/app".into(),
            args: vec!["literal; not shell".into()],
            timeout_ms: 100,
        }];
        flow.validate().unwrap();
        assert!(!flow.needs_keyboard());
        flow.steps = vec![Step::Launch {
            program: "/approved/app".into(),
            args: vec!["x".repeat(2049)],
            timeout_ms: 100,
        }];
        assert!(flow.validate().is_err());
    }
    #[test]
    fn fake_execution_and_cancel_never_launch_real_programs() {
        let mut host = FakeHost::default();
        let flow = Workflow {
            name: "fake".into(),
            steps: vec![
                Step::Media(MediaAction::Mute),
                Step::Launch {
                    program: "/fixture/not-executed".into(),
                    args: vec![],
                    timeout_ms: 10,
                },
            ],
        };
        let cancel = token();
        run(&flow, &mut host, &cancel).unwrap();
        assert_eq!((host.shortcuts, host.launches), (1, 1));
        cancel.epoch.fetch_add(1, Ordering::AcqRel);
        assert!(run(&flow, &mut host, &cancel).is_err());
        assert_eq!((host.shortcuts, host.launches), (1, 1));
    }
    #[test]
    fn bounded_queue_cancels_active_and_stale_work_and_joins() {
        let epoch = Arc::new(AtomicU64::new(0));
        let log = Arc::new(Mutex::new(Vec::new()));
        let observed = log.clone();
        let (started, rx) = mpsc::channel();
        let worker = Worker::spawn(epoch.clone(), move |id: usize, cancel| {
            observed.lock().unwrap().push(id);
            started.send(()).unwrap();
            if id == 0 {
                let _ = cancel.wait(Duration::from_secs(5));
            }
        });
        let submit = worker.submitter();
        submit.try_submit(0).unwrap();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        for id in 1..=QUEUE_CAPACITY {
            submit.try_submit(id).unwrap();
        }
        assert!(submit.try_submit(99).is_err());
        epoch.fetch_add(1, Ordering::AcqRel);
        // Keep the worker alive: it must discard old-generation pending jobs
        // rather than only losing them incidentally when dropped.
        let deadline = Instant::now() + Duration::from_secs(1);
        while submit.try_submit(100).is_err() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(*log.lock().unwrap(), vec![0, 100]);
        let before = Instant::now();
        drop(worker);
        assert!(before.elapsed() < Duration::from_secs(1));
        assert_eq!(*log.lock().unwrap(), vec![0, 100]);
        assert!(submit.try_submit(100).is_err());
    }
    #[test]
    fn errors_and_invalid_workflows_stop_before_later_steps() {
        struct ErrorHost;
        impl Host for ErrorHost {
            fn shortcut(&mut self, _: &KeyChord) -> Result<()> {
                bail!("fake input failure");
            }
            fn launch(
                &mut self,
                _: &str,
                _: &[String],
                _: Duration,
                _: &Cancellation,
            ) -> Result<()> {
                panic!("later launch must not run");
            }
        }
        let flow = Workflow {
            name: "failure".into(),
            steps: vec![
                Step::Media(MediaAction::Mute),
                Step::Launch {
                    program: "/fixture/not-executed".into(),
                    args: vec![],
                    timeout_ms: 10,
                },
            ],
        };
        assert!(run(&flow, &mut ErrorHost, &token()).is_err());
        let invalid = Workflow {
            name: "invalid".into(),
            steps: vec![Step::Shortcut(KeyChord::default())],
        };
        let mut fake = FakeHost::default();
        assert!(run(&invalid, &mut fake, &token()).is_err());
        assert_eq!(fake.shortcuts, 0);
    }
    #[test]
    fn cancellation_interrupts_delays_promptly() {
        let cancel = token();
        let epoch = cancel.epoch.clone();
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            tx.send(()).unwrap();
            run(
                &Workflow {
                    name: "delay".into(),
                    steps: vec![Step::Delay(10_000), Step::Media(MediaAction::Mute)],
                },
                &mut FakeHost::default(),
                &cancel,
            )
        });
        rx.recv().unwrap();
        epoch.fetch_add(1, Ordering::AcqRel);
        assert!(thread.join().unwrap().is_err());
    }
    #[test]
    fn locations_use_desktop_open_without_launching_a_bounded_command() {
        for uri in [
            "https://example.org/",
            "http://localhost/",
            "file:///home/user/report.pdf",
        ] {
            let flow = Workflow {
                name: "Open location".into(),
                steps: vec![Step::OpenUri(uri.into())],
            };
            flow.validate().unwrap();
            let mut host = FakeHost::default();
            run(&flow, &mut host, &token()).unwrap();
            assert_eq!((host.locations, host.launches, host.shortcuts), (1, 0, 0));
            assert!(!flow.needs_keyboard());
        }
        for uri in [
            "javascript:alert(1)",
            "file://remote/share",
            "/tmp/file",
            "https://example.org/\n",
        ] {
            assert!(validate_uri(uri).is_err(), "{uri}");
        }
    }
}
