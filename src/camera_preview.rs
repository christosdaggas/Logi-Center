//! Local, video-only thumbnail previews. Frames are never recorded or uploaded.
//! GTK owns textures; a cancellable worker owns each FFmpeg process and camera.
use std::cell::{Cell, RefCell};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk4 as gtk;
use libadwaita as adw;

use crate::peripherals::{self, Peripheral};

const WIDTH: i32 = 960;
const HEIGHT: i32 = 540;
const FRAME_BYTES: usize = WIDTH as usize * HEIGHT as usize * 3;
const ERROR_LIMIT: usize = 8192;

enum Event {
    Frame(Vec<u8>),
    Error(String),
}

struct Capture {
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<Event>>>,
    worker: Option<JoinHandle<()>>,
}

impl Capture {
    fn start(camera: Peripheral) -> Self {
        Self::spawn(move || {
            let (helper, path) = peripherals::camera_preview_source(&camera)?;
            let mut command = Command::new(helper);
            command
                .args([
                    "-nostdin",
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "v4l2",
                    "-framerate",
                    "10",
                    "-video_size",
                    "640x480",
                    "-i",
                ])
                .arg(path)
                .args([
                    "-an",
                    "-sn",
                    "-dn",
                    "-vf",
                    "fps=8,crop=640:360,scale=960:540",
                    "-threads",
                    "1",
                    "-filter_threads",
                    "1",
                    "-pix_fmt",
                    "rgb24",
                    "-f",
                    "rawvideo",
                    "pipe:1",
                ]);
            Ok(command)
        })
    }

    fn spawn(factory: impl FnOnce() -> anyhow::Result<Command> + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let latest = Arc::new(Mutex::new(None));
        let (cancel, output) = (stop.clone(), latest.clone());
        let worker = std::thread::spawn(move || {
            let result = factory().and_then(|command| read_frames(command, &cancel, &output));
            if !cancel.load(Ordering::Acquire) {
                if let Err(error) = result {
                    *output.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(Event::Error(format!("{error:#}")));
                }
            }
        });
        Self {
            stop,
            latest,
            worker: Some(worker),
        }
    }

    fn take(&self) -> Option<Event> {
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Never join a camera/helper on the GTK thread. The nonblocking worker
        // checks cancellation every 20 ms and kills/reaps only its own child.
        self.stop.store(true, Ordering::Release);
        self.worker.take();
    }
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        // The child has its own process group; no unrelated process is touched.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

fn nonblocking(fd: &impl AsRawFd) -> io::Result<()> {
    let fd = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn read_frames(
    mut command: Command,
    stop: &AtomicBool,
    output: &Mutex<Option<Event>>,
) -> anyhow::Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    // Prevent an orphaned capture after an application crash.
    let parent = std::process::id() as libc::pid_t;
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(io::Error::other("Preview parent exited"));
            }
            Ok(())
        });
    }
    let mut child = OwnedChild(command.spawn()?);
    let mut stdout = child.0.stdout.take().expect("piped preview stdout");
    let mut stderr = child.0.stderr.take().expect("piped preview stderr");
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    let mut frame = vec![0; FRAME_BYTES];
    let mut offset = 0;
    let mut errors = Vec::new();
    let mut last_frame = Instant::now();
    let mut received = false;
    while !stop.load(Ordering::Acquire) {
        let mut error_bytes = [0; 2048];
        if let Ok(size) = stderr.read(&mut error_bytes) {
            let remaining = ERROR_LIMIT.saturating_sub(errors.len());
            errors.extend_from_slice(&error_bytes[..size.min(remaining)]);
        }
        match stdout.read(&mut frame[offset..]) {
            Ok(0) => {
                // Drain the bounded diagnostic tail once after EOF.
                let _ = stderr
                    .by_ref()
                    .take(ERROR_LIMIT.saturating_sub(errors.len()) as u64)
                    .read_to_end(&mut errors);
                anyhow::bail!(
                    "{}",
                    preview_error(
                        &errors,
                        "Camera disconnected or preview ended. It may be busy in another app."
                    )
                );
            }
            Ok(size) => {
                offset += size;
                if offset == FRAME_BYTES {
                    *output.lock().unwrap_or_else(|e| e.into_inner()) = Some(Event::Frame(
                        std::mem::replace(&mut frame, vec![0; FRAME_BYTES]),
                    ));
                    offset = 0;
                    last_frame = Instant::now();
                    received = true;
                }
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
        if last_frame.elapsed() > Duration::from_secs(if received { 4 } else { 8 }) {
            anyhow::bail!(
                "{}",
                preview_error(
                    &errors,
                    "No preview frames received. Check permissions, camera privacy/shutter and whether another app is using it."
                )
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

fn preview_error(errors: &[u8], fallback: &str) -> String {
    let text = String::from_utf8_lossy(errors);
    let text = text.trim();
    if text.is_empty() {
        fallback.into()
    } else {
        text.into()
    }
}

struct PreviewCard {
    root: gtk::Box,
    picture: gtk::Picture,
    placeholder: gtk::Image,
    status: gtk::Label,
    pause: gtk::Button,
    camera: Peripheral,
    live: bool,
    paused: Cell<bool>,
    active: Cell<bool>,
    capture: RefCell<Option<Capture>>,
}

impl PreviewCard {
    fn new(camera: Peripheral, live: bool) -> Rc<Self> {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
        root.set_widget_name("camera-inline-preview");
        root.set_hexpand(true);
        let picture = gtk::Picture::new();
        picture.set_can_shrink(true);
        picture.set_keep_aspect_ratio(true);
        picture.set_hexpand(true);
        picture.set_vexpand(true);
        picture.set_widget_name("camera-preview-image");
        picture.set_alternative_text(Some(&format!("Live preview of {}", camera.name)));
        let overlay = gtk::Overlay::new();
        overlay.set_hexpand(true);
        overlay.set_vexpand(false);
        overlay.set_size_request(320, 480);
        overlay.add_css_class("camera-preview-surface");
        overlay.set_child(Some(&picture));
        let placeholder = gtk::Image::from_icon_name("camera-web-symbolic");
        placeholder.set_pixel_size(48);
        placeholder.set_halign(gtk::Align::Center);
        placeholder.set_valign(gtk::Align::Center);
        overlay.add_overlay(&placeholder);
        // A full-width surface with a generous height keeps the actual video
        // large; Picture preserves the 16:9 image inside the available width.
        root.append(&overlay);
        let status = gtk::Label::new(Some("Preview off · camera released"));
        status.set_xalign(0.0);
        status.set_wrap(true);
        status.set_lines(3);
        status.set_ellipsize(gtk::pango::EllipsizeMode::End);
        status.add_css_class("caption");
        status.add_css_class("camera-preview-status");
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        buttons.add_css_class("camera-preview-toolbar");
        status.set_hexpand(true);
        status.set_valign(gtk::Align::Center);
        buttons.append(&status);
        let pause = gtk::Button::with_label("Start preview");
        pause.add_css_class("camera-preview-button");
        pause.set_size_request(136, 38);
        pause.set_valign(gtk::Align::Center);
        pause.set_tooltip_text(Some(
            "Start or stop this preview; stopping releases the camera for other applications",
        ));
        buttons.append(&pause);
        root.append(&buttons);
        let card = Rc::new(Self {
            root,
            picture,
            placeholder,
            status,
            pause,
            camera,
            live,
            // Camera access is opt-in. Selection and hotplug never open a stream.
            paused: Cell::new(true),
            active: Cell::new(true),
            capture: RefCell::new(None),
        });
        let weak = Rc::downgrade(&card);
        card.root.connect_map(move |_| {
            if let Some(card) = weak.upgrade() {
                card.start();
            }
        });
        let weak = Rc::downgrade(&card);
        card.root.connect_unmap(move |_| {
            if let Some(card) = weak.upgrade() {
                card.stop();
            }
        });
        let weak = Rc::downgrade(&card);
        card.pause.connect_clicked(move |_| {
            if let Some(card) = weak.upgrade() {
                if card.capture.borrow().is_some() {
                    card.paused.set(true);
                    card.stop();
                } else {
                    card.paused.set(false);
                    card.start();
                }
            }
        });
        let weak = Rc::downgrade(&card);
        glib::timeout_add_local(Duration::from_millis(80), move || {
            let Some(card) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let event = card.capture.borrow().as_ref().and_then(Capture::take);
            match event {
                Some(Event::Frame(frame)) => card.frame(frame),
                Some(Event::Error(error)) => {
                    card.capture.borrow_mut().take();
                    card.clear();
                    card.status
                        .set_text(&format!("Preview unavailable: {error}"));
                    card.status.set_tooltip_text(Some(&error));
                    card.pause.set_label("Retry preview");
                }
                None => {}
            }
            glib::ControlFlow::Continue
        });
        card
    }

    fn start(&self) {
        if !self.active.get()
            || !self.root.is_mapped()
            || self.paused.get()
            || self.capture.borrow().is_some()
        {
            return;
        }
        self.pause.set_label("Pause preview");
        self.status.set_tooltip_text(None);
        if !self.live {
            self.status.set_text("Fixture preview · no camera opened");
            return;
        }
        self.status.set_text("Starting local preview…");
        *self.capture.borrow_mut() = Some(Capture::start(self.camera.clone()));
    }

    fn clear(&self) {
        self.picture.set_paintable(None::<&gtk::gdk::Paintable>);
        self.placeholder.set_visible(true);
    }

    fn stop(&self) {
        self.capture.borrow_mut().take();
        self.clear();
        self.status.set_text(if self.paused.get() {
            "Preview paused · camera released"
        } else {
            "Preview stopped"
        });
        self.pause.set_label(if self.paused.get() {
            "Resume preview"
        } else {
            "Pause preview"
        });
    }

    fn frame(&self, frame: Vec<u8>) {
        if frame.len() != FRAME_BYTES || !self.active.get() || !self.root.is_mapped() {
            return;
        }
        let texture = gtk::gdk::MemoryTexture::new(
            WIDTH,
            HEIGHT,
            gtk::gdk::MemoryFormat::R8g8b8,
            &glib::Bytes::from_owned(frame),
            WIDTH as usize * 3,
        );
        self.picture.set_paintable(Some(&texture));
        self.placeholder.set_visible(false);
        self.status.set_text("● Live · local video only · 8 fps");
    }
}

/// Embedded directly in one camera's settings page, never a separate overview.
pub struct CameraPreview {
    pub root: gtk::Box,
    panel: Rc<PreviewCard>,
}

impl CameraPreview {
    pub fn new(camera: &Peripheral, live: bool) -> Self {
        let panel = PreviewCard::new(camera.clone(), live);
        Self {
            root: panel.root.clone(),
            panel,
        }
    }

    pub fn matches(&self, camera: &Peripheral) -> bool {
        self.panel.camera.id == camera.id
            && peripherals::camera_key(&self.panel.camera) == peripherals::camera_key(camera)
            && serde_json::to_string(&self.panel.camera.controls).ok()
                == serde_json::to_string(&camera.controls).ok()
    }

    pub fn activate(&self) {
        self.panel.active.set(true);
        self.panel.start();
    }

    pub fn deactivate(&self) {
        self.panel.active.set(false);
        self.panel.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(capture: &Capture) -> Event {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(event) = capture.take() {
                return event;
            }
            assert!(Instant::now() < deadline, "Preview event timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn stop(mut capture: Capture) {
        capture.stop.store(true, Ordering::Release);
        if let Some(worker) = capture.worker.take() {
            worker.join().unwrap();
        }
    }

    #[test]
    fn preview_cancellation_kills_and_reaps_only_owned_child() {
        let capture = Capture::spawn(|| {
            let mut command = Command::new("/bin/sh");
            // Synthetic pixels only. PID is encoded in the frame for an exact
            // process-reaping assertion; no camera or filesystem output.
            command.args([
                "-c",
                &format!(
                    "printf '%016d' $$; dd if=/dev/zero bs={} count=1 2>/dev/null; exec sleep 30",
                    FRAME_BYTES - 16
                ),
            ]);
            Ok(command)
        });
        let Event::Frame(frame) = event(&capture) else {
            panic!("Expected a synthetic frame");
        };
        assert_eq!(frame.len(), FRAME_BYTES);
        let pid: i32 = std::str::from_utf8(&frame[..16]).unwrap().parse().unwrap();
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            0,
            "Positive control: child exists before cancellation"
        );
        let started = Instant::now();
        stop(capture);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "Cancellation must not wait for a blocked frame"
        );
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn preview_incomplete_frames_and_helper_errors_are_not_images() {
        let capture = Capture::spawn(|| {
            let mut command = Command::new("/bin/sh");
            command.args([
                "-c",
                "printf short; printf 'fixture camera busy' >&2; exit 1",
            ]);
            Ok(command)
        });
        let Event::Error(error) = event(&capture) else {
            panic!("Partial frame must not be rendered");
        };
        assert!(error.contains("fixture camera busy"));
        stop(capture);
        assert_eq!(preview_error(&[], "fallback"), "fallback");
    }

    #[test]
    fn preview_latest_frame_storage_is_bounded_and_source_ids_are_validated() {
        let capture = Capture::spawn(|| {
            let mut command = Command::new("/bin/sh");
            command.args([
                "-c",
                &format!("dd if=/dev/zero bs={FRAME_BYTES} count=4 2>/dev/null; exec sleep 30"),
            ]);
            Ok(command)
        });
        let Event::Frame(frame) = event(&capture) else {
            panic!("Expected a frame");
        };
        assert_eq!(frame.len(), FRAME_BYTES);
        // There is just one latest-frame slot, not an unbounded frame queue.
        let Event::Frame(frame) = event(&capture) else {
            panic!("Expected another complete frame");
        };
        assert_eq!(frame.len(), FRAME_BYTES);
        stop(capture);
        let fake: Peripheral = serde_json::from_value(serde_json::json!({
            "id":"camera:/tmp/not-a-camera", "name":"fake", "kind":"camera",
            "backend":"fixture", "status":"fixture", "metadata":{}, "controls":[]
        }))
        .unwrap();
        assert!(peripherals::camera_preview_source(&fake).is_err());
    }

    #[test]
    #[ignore = "explicitly opens connected camera video streams; no recording or control writes"]
    fn camera_preview_connected_cameras_deliver_and_release() {
        let inventory = peripherals::cameras();
        assert_eq!(
            inventory.devices.len(),
            2,
            "Expected the user's two connected cameras"
        );
        // A second complete capture pass proves release/reopen, not just receipt
        // of one cached frame. All frames remain in memory.
        for pass in 1..=2 {
            let captures: Vec<_> = inventory
                .devices
                .iter()
                .cloned()
                .map(Capture::start)
                .collect();
            for (camera, capture) in inventory.devices.iter().zip(&captures) {
                match event(capture) {
                    Event::Frame(frame) => {
                        assert_eq!(frame.len(), FRAME_BYTES);
                        println!(
                            "{}: live RGB frame {}x{}, pass {pass}",
                            camera.name, WIDTH, HEIGHT
                        );
                    }
                    Event::Error(error) => panic!("{}: {error}", camera.name),
                }
            }
            for capture in captures {
                stop(capture);
            }
        }
        println!("LIVE CAMERA PREVIEWS AND RELEASE PASSED");
    }
}
