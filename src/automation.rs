use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, Receiver, SyncSender},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::backend;
use crate::gesture::{Report, Stroke};
use crate::hidpp::Hidpp;
use crate::model::{DeviceSnapshot, ReceiverSnapshot};
use crate::profiles::{self, Action, Binding, DeviceProfile};
use crate::workflow::{self, Host};
use anyhow::{Context, Result, bail};

pub enum Event {
    Status(String, String),
    DpiChanged,
    /// The GTK main thread presents the ring; no action runs until chosen.
    Ring(DeviceSnapshot, Vec<(String, Action)>),
}
enum Command {
    Update,
    Reset,
    Stop,
    Execute(DeviceSnapshot, Action, u64),
}
type Configs = Vec<(DeviceSnapshot, DeviceProfile)>;

pub struct Supervisor {
    manager: Option<(SyncSender<Command>, JoinHandle<()>)>,
    events: SyncSender<Event>,
    epoch: Arc<AtomicU64>,
    pending: Arc<Mutex<Option<Configs>>>,
}

impl Supervisor {
    pub fn new() -> (Self, Receiver<Event>) {
        let (events, rx) = mpsc::sync_channel(64);
        (
            Self {
                manager: None,
                events,
                epoch: Arc::new(AtomicU64::new(0)),
                pending: Arc::new(Mutex::new(None)),
            },
            rx,
        )
    }
    pub fn reconcile(&mut self, receivers: &[ReceiverSnapshot]) {
        let configs: Vec<_> = receivers
            .iter()
            .flat_map(|receiver| &receiver.devices)
            .filter(|device| device.online && profiles::identity(device).is_some())
            .filter_map(|device| {
                let profile = profiles::load(device);
                (profile.actions_enabled
                    && profile.bindings.values().any(|binding| !binding.is_empty()))
                .then(|| (device.clone(), profile))
            })
            .collect();
        if configs.is_empty() && self.manager.is_none() {
            return;
        }
        self.ensure_manager();
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(configs);
        if let Some((tx, _)) = &self.manager {
            // Latest configuration lives outside the bounded command queue, so
            // even a saturated queue cannot lose a disable/reconfiguration.
            let _ = tx.try_send(Command::Update);
        }
    }
    pub fn shutdown(&mut self) {
        self.cancel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some((tx, thread)) = self.manager.take() {
            let _ = tx.send(Command::Stop);
            let _ = thread.join();
        }
    }
    pub fn reset(&self) {
        self.cancel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some((tx, _)) = &self.manager {
            let _ = tx.send(Command::Reset);
        }
    }

    fn ensure_manager(&mut self) {
        if self.manager.is_none() {
            let (tx, rx) = mpsc::sync_channel(32);
            let events = self.events.clone();
            let epoch = self.epoch.clone();
            let pending = self.pending.clone();
            self.manager = Some((
                tx,
                std::thread::spawn(move || manager(rx, events, epoch, pending)),
            ));
        }
    }

    /// User-selected ring/desktop action. Validation is synchronous; execution
    /// is queued, never performed on GTK or the HID reader. Errors use Status.
    pub fn execute(&mut self, device: DeviceSnapshot, action: Action) -> Result<()> {
        validate_action(&action)?;
        validate_device_action(&action, &device)?;
        if !device.online {
            bail!("The device is offline");
        }
        self.ensure_manager();
        self.manager
            .as_ref()
            .unwrap()
            .0
            .try_send(Command::Execute(
                device,
                action,
                self.epoch.load(Ordering::Acquire),
            ))
            .context("Action manager queue is full or stopped")
    }

    /// Cancels active and queued workflows for all devices, without waiting on
    /// a full queue. Subsequent physical/user actions are allowed to run.
    pub fn cancel(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
    }
}
impl Drop for Supervisor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Worker {
    signature: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    executor: workflow::Worker<Action>,
    suspended: Arc<AtomicBool>,
    device_key: String,
}
impl Worker {
    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        // Cancel macros before waiting for reporting restoration. No action
        // worker holds the hardware mutex across queue waits or GTK callbacks.
        drop(self.executor);
        if let Some(thread) = self.thread {
            let _ = thread.join();
        }
    }
}

fn manager(
    rx: Receiver<Command>,
    events: SyncSender<Event>,
    epoch: Arc<AtomicU64>,
    pending: Arc<Mutex<Option<Configs>>>,
) {
    let mut workers: HashMap<String, Worker> = HashMap::new();
    let mut deferred = None;
    loop {
        let mut command = if let Some(command) = deferred.take() {
            command
        } else {
            match rx.recv() {
                Ok(command) => command,
                Err(_) => break,
            }
        };
        if matches!(&command, Command::Execute(..))
            && pending.lock().unwrap_or_else(|e| e.into_inner()).is_some()
        {
            deferred = Some(command);
            command = Command::Update;
        }
        let configs = match command {
            Command::Update => {
                let Some(configs) = pending.lock().unwrap_or_else(|e| e.into_inner()).take() else {
                    continue;
                };
                configs
            }
            Command::Reset => {
                for (_, worker) in workers.drain() {
                    worker.stop();
                }
                continue;
            }
            Command::Stop => break,
            Command::Execute(device, action, generation) => {
                if generation != epoch.load(Ordering::Acquire) {
                    continue;
                }
                let key = profiles::identity(&device).unwrap_or_else(|| device.key());
                if workers
                    .get(&key)
                    .is_some_and(|worker| worker.device_key != device.key())
                {
                    if let Some(worker) = workers.remove(&key) {
                        worker.stop();
                    }
                }
                if !workers.contains_key(&key) {
                    if workers.len() >= 64 {
                        let _ = events.try_send(Event::Status(
                            device.key(),
                            "Too many action workers".into(),
                        ));
                        continue;
                    }
                    let profile = profiles::load(&device);
                    match start_worker(
                        String::new(),
                        device.clone(),
                        profile,
                        false,
                        &events,
                        &epoch,
                    ) {
                        Ok(worker) => {
                            workers.insert(key.clone(), worker);
                        }
                        Err(error) => {
                            let _ =
                                events.try_send(Event::Status(device.key(), format!("{error:#}")));
                            continue;
                        }
                    }
                }
                if let Err(error) = workers[&key]
                    .executor
                    .submitter()
                    .try_submit_generation(action, generation)
                {
                    let _ = events.try_send(Event::Status(device.key(), error.to_string()));
                }
                continue;
            }
        };
        let desired: HashMap<_, _> = configs
            .into_iter()
            .filter_map(|(device, profile)| {
                let key = profiles::identity(&device)?;
                let signature = format!(
                    "{}:{:?}:{}",
                    device.key(),
                    device.buttons_feature,
                    serde_json::to_string(&(
                        &profile.bindings,
                        profiles::preset_values(&profile, &device)
                    ))
                    .unwrap_or_default()
                );
                Some((key, (signature, device, profile)))
            })
            .collect();
        let obsolete: Vec<_> = workers
            .iter()
            .filter(|(key, worker)| {
                (worker.thread.as_ref().is_some_and(JoinHandle::is_finished)
                    && !worker.suspended.load(Ordering::Acquire))
                    || worker.executor.is_finished()
                    || desired
                        .get(*key)
                        .is_none_or(|(signature, _, _)| *signature != worker.signature)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in obsolete {
            if let Some(worker) = workers.remove(&key) {
                worker.stop();
            }
        }
        for (key, (signature, device, profile)) in desired {
            if workers.contains_key(&key) {
                continue;
            }
            match start_worker(signature, device.clone(), profile, true, &events, &epoch) {
                Ok(worker) => {
                    workers.insert(key, worker);
                }
                Err(error) => {
                    let _ = events.try_send(Event::Status(
                        device.key(),
                        format!("Custom actions inactive: {error:#}"),
                    ));
                }
            }
        }
    }
    for (_, worker) in workers {
        worker.stop();
    }
}

fn start_worker(
    signature: String,
    device: DeviceSnapshot,
    profile: DeviceProfile,
    divert: bool,
    events: &SyncSender<Event>,
    epoch: &Arc<AtomicU64>,
) -> Result<Worker> {
    if divert {
        for binding in profile.bindings.values() {
            binding.validate()?;
            for action in [&binding.tap, &binding.hold, &binding.double_tap]
                .into_iter()
                .flatten()
            {
                validate_action(action)?;
                validate_device_action(action, &device)?;
            }
        }
    }
    // Open input before the reporting thread can divert anything. Manual-only
    // workers open lazily on their executor, after explicit user selection.
    let needs_keyboard = divert
        && profile.bindings.values().any(|binding| {
            binding.gestures()
                || [&binding.tap, &binding.hold, &binding.double_tap]
                    .into_iter()
                    .flatten()
                    .any(action_needs_keyboard)
        });
    let keyboard = if needs_keyboard {
        Some(crate::input::Keyboard::open()?)
    } else {
        None
    };
    let mut host = workflow::SystemHost::new(keyboard);
    let device_key = device.key();
    let tx = events.clone();
    let action_device = device.clone();
    let presets = profiles::preset_values(&profile, &device);
    let stop = Arc::new(AtomicBool::new(false));
    let reporting_done = Arc::new(AtomicBool::new(!divert));
    let reporting_ok = Arc::new(AtomicBool::new(!divert));
    let suspended = Arc::new(AtomicBool::new(false));
    let action_suspended = suspended.clone();
    let action_stop = stop.clone();
    let action_done = reporting_done.clone();
    let action_reporting_ok = reporting_ok.clone();
    let executor = workflow::Worker::spawn(epoch.clone(), move |action, cancel| {
        let result = (|| {
            if matches!(action, Action::SwitchHost(_)) {
                // Restore owned diversions before the device leaves this host.
                // The reader never waits on us, and we hold no hardware lock.
                action_suspended.store(true, Ordering::Release);
                action_stop.store(true, Ordering::Release);
                let deadline = Instant::now() + Duration::from_secs(5);
                while !action_done.load(Ordering::Acquire) {
                    if Instant::now() >= deadline {
                        bail!("Timed out restoring button reporting; host was not switched");
                    }
                    cancel.wait(Duration::from_millis(10))?;
                }
                if !action_reporting_ok.load(Ordering::Acquire) {
                    bail!(
                        "Button reporting could not be verified as restored; host was not switched"
                    );
                }
            }
            dispatch(&action, &action_device, &presets, &mut host, &tx, cancel)
        })();
        if let Err(error) = result {
            let _ = tx.try_send(Event::Status(
                action_device.key(),
                format!("Action stopped: {error:#}"),
            ));
        }
    });
    let thread = if divert {
        let tx = events.clone();
        let worker_stop = stop.clone();
        let submit = executor.submitter();
        Some(std::thread::spawn(move || {
            let key = device.key();
            match run(device, profile, worker_stop, &tx, submit) {
                Ok(()) => reporting_ok.store(true, Ordering::Release),
                Err(error) => {
                    let _ = tx.try_send(Event::Status(
                        key,
                        format!("Custom actions inactive: {error:#}"),
                    ));
                }
            }
            reporting_done.store(true, Ordering::Release);
        }))
    } else {
        None
    };
    Ok(Worker {
        signature,
        stop,
        thread,
        executor,
        suspended,
        device_key,
    })
}

trait Reporting {
    fn read(&mut self, cid: u16) -> Result<u8>;
    fn write(&mut self, cid: u16, flags: u8) -> Result<()>;
}
struct Connection {
    hp: Hidpp,
    number: u8,
    index: u8,
}
impl Reporting for Connection {
    fn read(&mut self, cid: u16) -> Result<u8> {
        let reply = self
            .hp
            .read_feature(self.number, self.index, 0x20, &cid.to_be_bytes())
            .context("Could not read the original button reporting flags")?;
        if reply.len() < 5 || u16::from_be_bytes([reply[0], reply[1]]) != cid {
            bail!("Invalid button reporting reply");
        }
        Ok(reply[2])
    }
    fn write(&mut self, cid: u16, flags: u8) -> Result<()> {
        self.hp.write_feature(
            self.number,
            self.index,
            0x30,
            &[(cid >> 8) as u8, cid as u8, flags, 0, 0],
        )
    }
}
struct ReportingGuard<T: Reporting> {
    connection: T,
    original: Vec<(u16, u8)>,
    setting_up: bool,
}
impl ReportingGuard<Connection> {
    fn configure(device: &DeviceSnapshot, bindings: &BTreeMap<u16, Binding>) -> Result<Self> {
        let _lock = backend::hardware_lock();
        let mut hp = Hidpp::open(Path::new(&device.receiver_path))?;
        let index = hp
            .feature_index(device.number, backend::FEATURE_REPROG_CONTROLS_V4)?
            .context("The device does not support diverted button events")?;
        Self::setup(
            Connection {
                hp,
                number: device.number,
                index,
            },
            device,
            bindings,
        )
    }
}
impl<T: Reporting> ReportingGuard<T> {
    fn restore(&mut self) -> Result<()> {
        let _lock = (!self.setting_up).then(backend::hardware_lock);
        let mut failed = Vec::new();
        let mut error = None;
        for (cid, original) in self.original.drain(..) {
            let flags = 0x22 | (original & 0x11);
            if let Err(reason) = self.connection.write(cid, flags) {
                failed.push((cid, original));
                if error.is_none() {
                    error = Some(reason);
                }
            }
        }
        self.original = failed;
        if let Some(error) = error {
            Err(error).context("Could not restore owned button reporting")
        } else {
            Ok(())
        }
    }

    // The caller holds the hardware mutex and opens input before changing reporting.
    fn setup(
        connection: T,
        device: &DeviceSnapshot,
        bindings: &BTreeMap<u16, Binding>,
    ) -> Result<Self> {
        if bindings
            .values()
            .filter(|binding| binding.gestures())
            .count()
            > 1
        {
            bail!("Only one gesture button per device can own the RawXY stream");
        }
        let mut guard = Self {
            connection,
            original: Vec::new(),
            setting_up: true,
        };
        if bindings.values().any(Binding::gestures) {
            for button in device.buttons.iter().filter(|button| {
                button.raw_xy
                    && !bindings
                        .get(&button.cid)
                        .is_some_and(|binding| !binding.is_empty())
            }) {
                if guard.connection.read(button.cid)? & 0x10 != 0 {
                    bail!("RawXY reporting is already in use. Disable other gesture tools first");
                }
            }
        }
        let mut originals = Vec::new();
        // Validate and read ALL buttons before the first write.
        for (cid, binding) in bindings.iter().filter(|(_, binding)| !binding.is_empty()) {
            let button = device
                .buttons
                .iter()
                .find(|button| button.cid == *cid && button.divertable)
                .context("The selected button is not divertable")?;
            if binding.gestures() && !button.raw_xy {
                bail!("The selected button does not support RawXY gestures");
            }
            binding.validate()?;
            let original = guard.connection.read(*cid)?;
            if original & 0x55 != 0 {
                bail!("Button reporting is already diverted. Disable its existing diversion first");
            }
            originals.push((*cid, original));
        }
        for (cid, original) in originals {
            guard.original.push((cid, original));
            let flags = if bindings[&cid].gestures() {
                0x33
            } else {
                0x23
            };
            guard.connection.write(cid, flags)?;
        }
        guard.setting_up = false;
        Ok(guard)
    }
}
impl<T: Reporting> Drop for ReportingGuard<T> {
    fn drop(&mut self) {
        if !self.original.is_empty() {
            if let Err(error) = self.restore() {
                log::warn!("{error:#}");
            }
        }
    }
}

fn run(
    device: DeviceSnapshot,
    profile: DeviceProfile,
    stop: Arc<AtomicBool>,
    events: &SyncSender<Event>,
    submit: workflow::Submitter<Action>,
) -> Result<()> {
    let (_, mut reader, mut reporting) = prepare(
        || Ok(()), // The action worker's keyboard was already opened.
        || crate::hidraw::Hidraw::open(Path::new(&device.receiver_path)),
        || {
            if stop.load(Ordering::Relaxed) {
                bail!("Custom action setup was cancelled");
            }
            ReportingGuard::configure(&device, &profile.bindings)
        },
    )?;
    let _ = events.try_send(Event::Status(
        device.key(),
        crate::i18n::tr("Custom actions active for {device}").replace("{device}", &device.name),
    ));
    let mut router = Router::default();
    let mut generation = submit.generation();
    while !stop.load(Ordering::Relaxed) {
        if submit.is_stopped() {
            bail!("Action worker stopped; restoring button reporting");
        }
        let current = submit.generation();
        if generation != current {
            router.cancel_pending();
            generation = current;
        }
        // Timers are serviced even without motion/button reports. The 10 ms
        // poll bounds scheduling jitter (subject to OS scheduling, not realtime).
        let report = reader
            .read_report(Duration::from_millis(10))?
            .and_then(|raw| {
                crate::gesture::decode(&raw, device.number, reporting.connection.index)
            });
        let actions = match report {
            Some(report) => router.route(report, &profile.bindings, Instant::now()),
            None => router.tick(&profile.bindings, Instant::now()),
        };
        for action in actions {
            if let Err(error) = submit.try_submit_generation(action, generation) {
                let _ = events.try_send(Event::Status(device.key(), error.to_string()));
            }
        }
    }
    reporting.restore()
}

// Never divert a button until all required input endpoints have opened successfully.
fn prepare<I, R, G>(
    input: impl FnOnce() -> Result<I>,
    reader: impl FnOnce() -> Result<R>,
    reporting: impl FnOnce() -> Result<G>,
) -> Result<(I, R, G)> {
    let input = input()?;
    let reader = reader()?;
    let reporting = reporting()?;
    Ok((input, reader, reporting))
}

#[derive(Default)]
struct Router {
    held: HashSet<u16>,
    strokes: BTreeMap<u16, Press>,
    pending: BTreeMap<u16, (Instant, Option<Action>)>,
}
struct Press {
    stroke: Stroke,
    start: Instant,
    consumed: bool,
}
impl Router {
    fn cancel_pending(&mut self) {
        self.pending.clear();
        // Require release before an already-held button may trigger again.
        for press in self.strokes.values_mut() {
            press.consumed = true;
        }
    }
    fn tick(&mut self, bindings: &BTreeMap<u16, Binding>, time: Instant) -> Vec<Action> {
        let mut actions = Vec::new();
        self.pending.retain(|cid, (released, action)| {
            let Some(binding) = bindings.get(cid) else {
                return false;
            };
            if time.saturating_duration_since(*released)
                > Duration::from_millis(u64::from(binding.double_tap_ms))
            {
                if let Some(action) = action {
                    actions.push(action.clone());
                }
                false
            } else {
                true
            }
        });
        for (cid, press) in &mut self.strokes {
            let Some(binding) = bindings.get(cid) else {
                continue;
            };
            if press.consumed {
                continue;
            }
            let elapsed = time.saturating_duration_since(press.start);
            if elapsed >= Duration::from_secs(30) {
                press.consumed = true;
                self.pending.remove(cid);
            } else if elapsed >= Duration::from_millis(u64::from(binding.hold_ms))
                && press.stroke.tap()
                && let Some(action) = &binding.hold
            {
                actions.push(action.clone());
                press.consumed = true;
                self.pending.remove(cid);
            }
        }
        actions
    }

    fn route(
        &mut self,
        report: Report,
        bindings: &BTreeMap<u16, Binding>,
        time: Instant,
    ) -> Vec<Action> {
        // Process releases/motion before the hold timer, so movement at the
        // threshold wins over a hold. Flush expired singles before new presses.
        let mut actions = Vec::new();
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter_map(|(cid, (released, _))| {
                let binding = bindings.get(cid)?;
                (time.saturating_duration_since(*released)
                    > Duration::from_millis(u64::from(binding.double_tap_ms)))
                .then_some(*cid)
            })
            .collect();
        for cid in expired {
            if let Some((_, Some(action))) = self.pending.remove(&cid) {
                actions.push(action);
            }
        }
        match report {
            Report::Buttons(cids) => {
                let now: HashSet<_> = cids.into_iter().collect();
                for (cid, binding) in bindings {
                    if now.contains(cid) && !self.held.contains(cid) && !binding.is_empty() {
                        self.strokes.insert(
                            *cid,
                            Press {
                                stroke: Stroke::new(binding.gesture.clone()),
                                start: time,
                                consumed: false,
                            },
                        );
                    }
                    if self.held.contains(cid) && !now.contains(cid) {
                        let Some(press) = self.strokes.remove(cid) else {
                            continue;
                        };
                        if press.consumed || !press.stroke.tap() {
                            self.pending.remove(cid);
                            continue;
                        }
                        let elapsed = time.saturating_duration_since(press.start);
                        if elapsed >= Duration::from_secs(30) {
                            self.pending.remove(cid);
                            continue;
                        }
                        if elapsed >= Duration::from_millis(u64::from(binding.hold_ms))
                            && let Some(action) = &binding.hold
                        {
                            actions.push(action.clone());
                            self.pending.remove(cid);
                        } else if let Some(double) = &binding.double_tap {
                            if self.pending.remove(cid).is_some() {
                                actions.push(double.clone());
                            } else {
                                // Retain even a no-op first tap, so double-only
                                // bindings can be recognized without a shortcut.
                                self.pending.insert(*cid, (time, binding.tap.clone()));
                            }
                        } else if let Some(action) = &binding.tap {
                            actions.push(action.clone());
                        }
                    }
                }
                self.held = now;
            }
            Report::Motion(dx, dy) => {
                // RawXY reports have no CID. Exactly one configured owner may
                // receive them; never broadcast movement to all held buttons.
                let owner = bindings
                    .iter()
                    .find(|(_, binding)| binding.gestures())
                    .map(|(cid, _)| *cid);
                if let Some((cid, press)) =
                    owner.and_then(|cid| self.strokes.get_mut(&cid).map(|press| (cid, press)))
                {
                    if !press.consumed
                        && let Some(direction) = press.stroke.sample(
                            dx,
                            dy,
                            time.saturating_duration_since(press.start).as_millis(),
                        )
                    {
                        self.pending.remove(&cid);
                        if let Some(chord) = &bindings[&cid].directions[direction as usize] {
                            actions.push(Action::Shortcut(chord.clone()));
                        }
                    }
                }
            }
        }
        actions.extend(self.tick(bindings, time));
        actions
    }
}

pub fn action_needs_keyboard(action: &Action) -> bool {
    match action {
        Action::Shortcut(_) => true,
        Action::Workflow(flow) => flow.needs_keyboard(),
        // A ring only requests GTK; selected actions get their own endpoint.
        Action::Ring(_) | Action::CycleDpi | Action::SwitchHost(_) => false,
    }
}

/// Shared execution/editor validation, including bounded ring recursion and
/// aggregate queue payload size. Profiles validate the recursive action tree.
pub fn validate_action(action: &Action) -> Result<()> {
    action.validate()?;
    if serde_json::to_vec(action)?.len() > 262_144 {
        bail!("Action exceeds the 256 KiB queue payload limit");
    }
    Ok(())
}

fn validate_device_action(action: &Action, device: &DeviceSnapshot) -> Result<()> {
    match action {
        Action::CycleDpi if device.dpi_feature.is_none() => {
            bail!("This device has no adjustable DPI")
        }
        Action::SwitchHost(host)
            if device.host_feature.is_none()
                || device
                    .host
                    .as_ref()
                    .is_none_or(|info| *host == 0 || *host > info.count) =>
        {
            bail!("This host slot is not available on the device")
        }
        Action::Ring(items) => {
            for (_, action) in items {
                validate_device_action(action, device)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn dispatch(
    action: &Action,
    device: &DeviceSnapshot,
    presets: &[u16],
    host: &mut workflow::SystemHost,
    events: &SyncSender<Event>,
    cancel: &workflow::Cancellation,
) -> Result<()> {
    validate_action(action)?;
    validate_device_action(action, device)?;
    cancel.check()?;
    match action {
        Action::Shortcut(chord) => host.shortcut(chord),
        Action::Workflow(flow) => workflow::run(flow, host, cancel),
        Action::Ring(items) => {
            let _ = events.try_send(Event::Ring(device.clone(), items.clone()));
            Ok(())
        }
        Action::SwitchHost(slot) => {
            cancel.check()?;
            let result = backend::switch_hosts(std::slice::from_ref(device), *slot - 1)?;
            if let Some(failed) = result
                .devices
                .iter()
                .find(|item| item.state != backend::HostSwitchState::SentUnconfirmed)
            {
                bail!(
                    "{}; custom actions remain paused until reset/reconnect",
                    failed
                        .error
                        .as_deref()
                        .unwrap_or("Host switch was not sent")
                );
            }
            let _ = events.try_send(Event::Status(
                device.key(),
                "Host switch sent, not confirmed. Custom actions paused until reconnect/reset."
                    .into(),
            ));
            Ok(())
        }
        Action::CycleDpi => {
            let profile_name = profiles::selection_status(device)?.effective;
            let current = {
                let _lock = backend::hardware_lock();
                let mut hp = Hidpp::open(Path::new(&device.receiver_path))?;
                let index = hp
                    .feature_index(device.number, backend::FEATURE_ADJUSTABLE_DPI)?
                    .context("This device does not support adjustable DPI")?;
                let reply = hp
                    .read_feature(device.number, index, 0x20, &[])
                    .context("Could not read current DPI")?;
                crate::dpi::current(&reply).context("Invalid current DPI reply")?
            };
            let next = crate::dpi::next(presets, current).context("No valid DPI presets")?;
            cancel.check()?;
            backend::apply_settings(
                device,
                &backend::SettingsUpdate {
                    dpi: Some(next),
                    ..Default::default()
                },
            )?;
            profiles::save_runtime_dpi_to(device, &profile_name, next)?;
            let _ = events.try_send(Event::DpiChanged);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ButtonInfo;
    use crate::profiles::KeyChord;
    use std::sync::Mutex;

    #[test]
    fn input_permission_failures_never_reach_diversion_setup() {
        assert!(
            prepare(
                || -> Result<()> {
                    anyhow::bail!("uinput permission denied");
                },
                || Ok(()),
                || -> Result<()> {
                    panic!("must not divert");
                }
            )
            .is_err()
        );
        assert!(
            prepare(
                || Ok(()),
                || -> Result<()> {
                    anyhow::bail!("hidraw permission denied");
                },
                || -> Result<()> {
                    panic!("must not divert");
                }
            )
            .is_err()
        );
        let order = std::cell::RefCell::new(Vec::new());
        prepare(
            || {
                order.borrow_mut().push("input");
                Ok(())
            },
            || {
                order.borrow_mut().push("reader");
                Ok(())
            },
            || {
                order.borrow_mut().push("reporting");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*order.borrow(), vec!["input", "reader", "reporting"]);
    }

    fn chord() -> KeyChord {
        KeyChord {
            keys: vec![29, 46],
            label: "Ctrl+C".into(),
        }
    }
    fn fixture() -> (DeviceSnapshot, BTreeMap<u16, Binding>) {
        let device = DeviceSnapshot {
            buttons: vec![1, 2]
                .into_iter()
                .map(|cid| ButtonInfo {
                    cid,
                    divertable: true,
                    raw_xy: true,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let bindings = [
            (
                1,
                Binding {
                    tap: Some(Action::CycleDpi),
                    ..Default::default()
                },
            ),
            (
                2,
                Binding {
                    tap: Some(Action::Shortcut(chord())),
                    directions: [None, None, None, Some(chord())],
                    ..Default::default()
                },
            ),
        ]
        .into_iter()
        .collect();
        (device, bindings)
    }
    #[test]
    fn routing_taps_repeated_reports_gestures_and_release() {
        let (_, bindings) = fixture();
        let mut router = Router::default();
        let now = Instant::now();
        assert!(
            router
                .route(Report::Buttons(vec![]), &bindings, now)
                .is_empty()
        );
        assert!(
            router
                .route(Report::Buttons(vec![1]), &bindings, now)
                .is_empty()
        );
        assert!(
            router
                .route(Report::Buttons(vec![1]), &bindings, now)
                .is_empty()
        );
        assert_eq!(
            router.route(Report::Buttons(vec![]), &bindings, now),
            vec![Action::CycleDpi]
        );
        assert!(
            router
                .route(Report::Buttons(vec![]), &bindings, now)
                .is_empty()
        );
        router.route(Report::Buttons(vec![2]), &bindings, now);
        assert_eq!(
            router.route(Report::Buttons(vec![]), &bindings, now),
            vec![Action::Shortcut(chord())]
        );
        router.route(Report::Buttons(vec![2]), &bindings, now);
        for _ in 0..2 {
            assert!(
                router
                    .route(Report::Motion(20, 0), &bindings, now)
                    .is_empty()
            );
        }
        assert_eq!(
            router.route(Report::Motion(20, 0), &bindings, now),
            vec![Action::Shortcut(chord())]
        );
        assert!(
            router
                .route(Report::Motion(-100, 0), &bindings, now)
                .is_empty()
        );
        assert!(
            router
                .route(Report::Buttons(vec![]), &bindings, now)
                .is_empty()
        );
        assert!(router.strokes.is_empty());
        assert!(
            router
                .route(Report::Motion(100, 0), &bindings, now)
                .is_empty()
        );
    }

    #[test]
    fn hold_and_double_tap_timers_do_not_require_new_reports() {
        let now = Instant::now();
        let at = |ms| now + Duration::from_millis(ms);
        let mut bindings = BTreeMap::from([(
            1,
            Binding {
                tap: Some(Action::CycleDpi),
                hold: Some(Action::Shortcut(chord())),
                double_tap: Some(Action::SwitchHost(2)),
                ..Default::default()
            },
        )]);
        let mut router = Router::default();
        router.route(Report::Buttons(vec![1]), &bindings, at(0));
        assert!(router.tick(&bindings, at(599)).is_empty());
        assert_eq!(
            router.tick(&bindings, at(600)),
            vec![Action::Shortcut(chord())]
        );
        assert!(router.tick(&bindings, at(610)).is_empty());
        assert!(
            router
                .route(Report::Buttons(vec![]), &bindings, at(700))
                .is_empty()
        );
        router.route(Report::Buttons(vec![1]), &bindings, at(1000));
        assert!(
            router
                .route(Report::Buttons(vec![]), &bindings, at(1010))
                .is_empty()
        );
        router.route(Report::Buttons(vec![1]), &bindings, at(1100));
        assert_eq!(
            router.route(Report::Buttons(vec![]), &bindings, at(1200)),
            vec![Action::SwitchHost(2)]
        );
        assert!(router.tick(&bindings, at(1500)).is_empty());
        router.route(Report::Buttons(vec![1]), &bindings, at(2000));
        router.route(Report::Buttons(vec![]), &bindings, at(2010));
        assert!(router.tick(&bindings, at(2260)).is_empty());
        assert_eq!(router.tick(&bindings, at(2261)), vec![Action::CycleDpi]);
        // A double-only binding must not manufacture a no-op ring/single.
        bindings.get_mut(&1).unwrap().tap = None;
        router.route(Report::Buttons(vec![1]), &bindings, at(3000));
        router.route(Report::Buttons(vec![]), &bindings, at(3010));
        assert!(router.tick(&bindings, at(3261)).is_empty());
        router.route(Report::Buttons(vec![1]), &bindings, at(4000));
        router.route(Report::Buttons(vec![]), &bindings, at(4010));
        router.route(Report::Buttons(vec![1]), &bindings, at(4100));
        assert_eq!(
            router.route(Report::Buttons(vec![]), &bindings, at(4110)),
            vec![Action::SwitchHost(2)]
        );
    }

    #[test]
    fn gesture_jitter_suppresses_accidental_taps_holds_and_other_buttons() {
        let (_, mut bindings) = fixture();
        bindings.get_mut(&2).unwrap().hold = Some(Action::SwitchHost(1));
        bindings.get_mut(&1).unwrap().hold = Some(Action::SwitchHost(2));
        let mut router = Router::default();
        let now = Instant::now();
        router.route(Report::Buttons(vec![1, 2]), &bindings, now);
        assert!(
            router
                .route(
                    Report::Motion(15, 0),
                    &bindings,
                    now + Duration::from_millis(10)
                )
                .is_empty()
        );
        // Only the RawXY owner sees movement. Button 1 still performs its hold.
        assert_eq!(
            router.tick(&bindings, now + Duration::from_millis(600)),
            vec![Action::SwitchHost(2)]
        );
        assert!(
            router
                .route(
                    Report::Buttons(vec![]),
                    &bindings,
                    now + Duration::from_millis(610)
                )
                .is_empty()
        );
        assert!(router.strokes.is_empty());
        router.route(
            Report::Buttons(vec![2]),
            &bindings,
            now + Duration::from_millis(1000),
        );
        router.route(
            Report::Motion(1, -1),
            &bindings,
            now + Duration::from_millis(1010),
        );
        assert_eq!(
            router.tick(&bindings, now + Duration::from_millis(1600)),
            vec![Action::SwitchHost(1)]
        );
        // A stale held report cannot trigger after the 30-second safety cutoff.
        router.route(
            Report::Buttons(vec![]),
            &bindings,
            now + Duration::from_millis(1700),
        );
        router.route(
            Report::Buttons(vec![2]),
            &bindings,
            now + Duration::from_millis(2000),
        );
        assert!(
            router
                .tick(&bindings, now + Duration::from_secs(33))
                .is_empty()
        );
        assert!(
            router
                .route(
                    Report::Buttons(vec![]),
                    &bindings,
                    now + Duration::from_secs(34)
                )
                .is_empty()
        );
    }

    #[test]
    fn ring_validation_and_public_execute_reject_without_side_effects() {
        let (mut supervisor, _) = Supervisor::new();
        assert!(validate_action(&Action::Ring(vec![])).is_err());
        assert!(
            validate_action(&Action::Ring(vec![(
                "Copy".into(),
                Action::Shortcut(chord())
            )]))
            .is_ok()
        );
        let device = DeviceSnapshot::default();
        assert!(
            supervisor
                .execute(device.clone(), Action::Shortcut(KeyChord::default()))
                .is_err()
        );
        assert!(supervisor.execute(device, Action::CycleDpi).is_err());
        assert!(
            supervisor.manager.is_none(),
            "rejected actions cannot start workers or hardware/input access"
        );
        let before = supervisor.epoch.load(Ordering::Acquire);
        supervisor.cancel();
        assert_ne!(before, supervisor.epoch.load(Ordering::Acquire));
    }

    #[test]
    fn two_rawxy_owners_are_rejected_before_any_reporting_write() {
        let (device, mut bindings) = fixture();
        bindings.get_mut(&1).unwrap().directions[0] = Some(chord());
        let writes = Arc::new(Mutex::new(Vec::new()));
        assert!(
            ReportingGuard::setup(
                Fake {
                    writes: writes.clone(),
                    fail: None,
                    original: 0
                },
                &device,
                &bindings
            )
            .is_err()
        );
        assert!(writes.lock().unwrap().is_empty());
    }

    #[test]
    fn cancellation_clears_single_tap_timers_and_consumes_existing_holds() {
        let now = Instant::now();
        let bindings = BTreeMap::from([(
            1,
            Binding {
                tap: Some(Action::CycleDpi),
                hold: Some(Action::SwitchHost(1)),
                double_tap: Some(Action::SwitchHost(2)),
                ..Default::default()
            },
        )]);
        let mut router = Router::default();
        router.route(Report::Buttons(vec![1]), &bindings, now);
        router.route(
            Report::Buttons(vec![]),
            &bindings,
            now + Duration::from_millis(10),
        );
        router.route(
            Report::Buttons(vec![1]),
            &bindings,
            now + Duration::from_millis(100),
        );
        router.cancel_pending();
        assert!(
            router
                .tick(&bindings, now + Duration::from_secs(1))
                .is_empty()
        );
        assert!(
            router
                .route(
                    Report::Buttons(vec![]),
                    &bindings,
                    now + Duration::from_secs(2)
                )
                .is_empty()
        );
        router.route(
            Report::Buttons(vec![1]),
            &bindings,
            now + Duration::from_secs(3),
        );
        assert_eq!(
            router.tick(&bindings, now + Duration::from_millis(3600)),
            vec![Action::SwitchHost(1)]
        );
    }

    #[test]
    fn saturated_command_queue_never_loses_disable_and_events_are_bounded() {
        let (mut supervisor, _events) = Supervisor::new();
        let (tx, rx) = mpsc::sync_channel(1);
        assert!(tx.try_send(Command::Reset).is_ok());
        // A fake manager does not consume the saturated command queue.
        supervisor.manager = Some((tx, std::thread::spawn(|| {})));
        supervisor.reconcile(&[]);
        assert!(
            supervisor
                .pending
                .lock()
                .unwrap()
                .take()
                .is_some_and(|configs| configs.is_empty())
        );
        assert!(matches!(rx.try_recv(), Ok(Command::Reset)));
        let (tx, thread) = supervisor.manager.take().unwrap();
        drop(tx);
        drop(rx);
        thread.join().unwrap();
        for _ in 0..64 {
            assert!(supervisor.events.try_send(Event::DpiChanged).is_ok());
        }
        assert!(supervisor.events.try_send(Event::DpiChanged).is_err());
        supervisor.cancel(); // Must not wait for room in either bounded queue.
    }

    struct Fake {
        writes: Arc<Mutex<Vec<(u16, u8)>>>,
        fail: Option<u16>,
        original: u8,
    }
    impl Reporting for Fake {
        fn read(&mut self, _: u16) -> Result<u8> {
            Ok(self.original)
        }
        fn write(&mut self, cid: u16, flags: u8) -> Result<()> {
            self.writes.lock().unwrap().push((cid, flags));
            if flags & 1 != 0 && self.fail == Some(cid) {
                bail!("simulated device error");
            }
            Ok(())
        }
    }
    #[test]
    fn setup_and_partial_failure_revert_only_owned_reporting() {
        let (device, bindings) = fixture();
        let writes = Arc::new(Mutex::new(Vec::new()));
        {
            let _lock = backend::hardware_lock();
            let guard = ReportingGuard::setup(
                Fake {
                    writes: writes.clone(),
                    fail: None,
                    original: 0,
                },
                &device,
                &bindings,
            )
            .unwrap();
            assert_eq!(*writes.lock().unwrap(), vec![(1, 0x23), (2, 0x33)]);
            drop(_lock);
            drop(guard);
        }
        assert_eq!(
            *writes.lock().unwrap(),
            vec![(1, 0x23), (2, 0x33), (1, 0x22), (2, 0x22)]
        );
        writes.lock().unwrap().clear();
        let _lock = backend::hardware_lock();
        assert!(
            ReportingGuard::setup(
                Fake {
                    writes: writes.clone(),
                    fail: Some(2),
                    original: 0
                },
                &device,
                &bindings
            )
            .is_err()
        );
        assert_eq!(
            *writes.lock().unwrap(),
            vec![(1, 0x23), (2, 0x33), (1, 0x22), (2, 0x22)]
        );
    }
    #[test]
    fn invalid_bindings_and_external_diversions_never_change_hardware() {
        let (mut device, mut bindings) = fixture();
        let writes = Arc::new(Mutex::new(Vec::new()));
        let _lock = backend::hardware_lock();
        assert!(
            ReportingGuard::setup(
                Fake {
                    writes: writes.clone(),
                    fail: None,
                    original: 1
                },
                &device,
                &bindings
            )
            .is_err()
        );
        device.buttons[1].raw_xy = false;
        assert!(
            ReportingGuard::setup(
                Fake {
                    writes: writes.clone(),
                    fail: None,
                    original: 0
                },
                &device,
                &bindings
            )
            .is_err()
        );
        bindings.get_mut(&1).unwrap().tap = Some(Action::Shortcut(KeyChord::default()));
        assert!(
            ReportingGuard::setup(
                Fake {
                    writes: writes.clone(),
                    fail: None,
                    original: 0
                },
                &device,
                &bindings
            )
            .is_err()
        );
        assert!(writes.lock().unwrap().is_empty());
    }
}
