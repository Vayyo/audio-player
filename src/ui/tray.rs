//! Linux StatusNotifierItem: the audio stream belongs to the app, not its window.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use eframe::egui::{self, ViewportCommand, ViewportId};
use ksni::blocking::TrayMethods;
use ksni::menu::StandardItem;

type Handle = ksni::blocking::Handle<RapTray>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Move {
    Hide,
    Show,
}

enum Event {
    Ready(Handle),
    Show,
    Quit,
    Moved { action: Move, hidden: bool, error: Option<String> },
}

struct RapTray {
    icon: Vec<ksni::Icon>,
    sender: mpsc::Sender<Event>,
    context: egui::Context,
    registered: Arc<AtomicBool>,
    worker: Option<mpsc::Sender<Move>>,
    quitting: Arc<AtomicBool>,
}

impl RapTray {
    fn notify(&self, event: Event) -> bool {
        let sent = self.sender.send(event).is_ok();
        self.context.request_repaint();
        sent
    }

    fn show(&self) {
        if self.quitting.load(Ordering::Acquire) || !self.notify(Event::Show) {
            return;
        }
        if let Some(worker) = &self.worker
            && worker.send(Move::Show).is_err()
        {
            eprintln!("Rap workspace worker stopped before restoring the window");
            std::process::exit(1); // No worker can recover an offscreen window.
        }
    }

    fn quit(&self) {
        if self.quitting.swap(true, Ordering::AcqRel) {
            return;
        }
        // Explicit Quit must not depend on an offscreen viewport repaint or on
        // Hyprland's close dispatcher. C exit handlers can race the render thread
        // (notably NVIDIA GLX); playback/settings are in memory and the OS reclaims
        // audio, Wayland and D-Bus resources on process termination.
        // SAFETY: This intentionally skips Rust/C cleanup; no persisted data needs
        // flushing, and the kernel releases all process-owned resources.
        unsafe { libc::_exit(0) }
    }
}

impl ksni::Tray for RapTray {
    fn id(&self) -> String {
        "rap".into()
    }

    fn title(&self) -> String {
        "Rap".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        self.icon.clone()
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.show();
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Show".into(),
                activate: Box::new(|tray: &mut Self| tray.show()),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|tray: &mut Self| tray.quit()),
                ..Default::default()
            }
            .into(),
        ]
    }

    fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
        // Restore on the workspace worker: hidden windows may never repaint.
        self.registered.store(false, Ordering::Release);
        if self.quitting.load(Ordering::Acquire) {
            return false;
        }
        self.show();
        if std::env::var_os("WAYLAND_DISPLAY").is_none() {
            self.context
                .send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(true));
        }
        false // Shut down; a later close will exit normally.
    }
}

/// The pixmap is computed once on the registration worker, not on UI frames.
/// ksni expects alpha, red, green, blue bytes in that order.
fn icon() -> ksni::Icon {
    const SIZE: usize = 32;
    let mut data = vec![0; SIZE * SIZE * 4];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as i32 - 16;
            let dy = y as i32 - 16;
            let circle = dx * dx + dy * dy <= 225;
            let note = (19..=21).contains(&x) && (8..=21).contains(&y)
                || (14..=21).contains(&x) && (8..=10).contains(&y)
                || (13..=19).contains(&x) && (19..=23).contains(&y)
                || (9..=15).contains(&x) && (21..=25).contains(&y);
            let color = if note && circle {
                [255, 255, 188, 101]
            } else if circle {
                [255, 43, 34, 58]
            } else {
                [0, 0, 0, 0]
            };
            data[(y * SIZE + x) * 4..(y * SIZE + x + 1) * 4].copy_from_slice(&color);
        }
    }
    ksni::Icon {
        width: SIZE as i32,
        height: SIZE as i32,
        data,
    }
}

pub(super) struct TrayService {
    receiver: mpsc::Receiver<Event>,
    handle: Option<Handle>,
    registered: Arc<AtomicBool>,
    worker: Option<mpsc::Sender<Move>>,
    pending: Option<Move>,
    hidden: bool,
    want_visible: bool,
    raise: bool,
    retry_at: Option<Instant>,
    quitting: bool,
}

impl TrayService {
    pub(super) fn start(context: &egui::Context) -> Self {
        let (sender, receiver) = mpsc::channel();
        let registered = Arc::new(AtomicBool::new(true));
        let quitting = Arc::new(AtomicBool::new(false));
        let worker = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            hyprland_worker(&sender, context, &registered, &quitting)
        } else {
            None
        };
        let tray_worker = worker.clone();
        let status = Arc::clone(&registered);
        let context = context.clone();
        let registration = std::thread::Builder::new()
            .name("rap-tray-register".into())
            .spawn(move || {
                let tray = RapTray {
                    icon: vec![icon()],
                    sender: sender.clone(),
                    context: context.clone(),
                    registered: status,
                    worker: tray_worker,
                    quitting,
                };
                match tray.spawn() {
                    Ok(handle) => {
                        if let Err(mpsc::SendError(Event::Ready(handle))) = sender.send(Event::Ready(handle)) {
                            // The UI has already closed; shut down the service.
                            std::mem::drop(handle.shutdown());
                        }
                        context.request_repaint();
                    }
                    Err(err) => eprintln!("Rap tray unavailable: {err}"),
                }
            });
        if let Err(err) = registration {
            eprintln!("Rap tray worker unavailable: {err}");
        }
        Self {
            receiver, handle: None, registered, worker, pending: None,
            hidden: false, want_visible: true, raise: false, retry_at: None, quitting: false,
        }
    }

    fn active(&self) -> bool {
        self.registered.load(Ordering::Acquire)
            && self.handle.as_ref().is_some_and(|handle| !handle.is_closed())
    }

    pub(super) fn update(&mut self, context: &egui::Context) {
        while let Ok(event) = self.receiver.try_recv() {
            match event {
                Event::Ready(handle) => self.handle = Some(handle),
                Event::Show if !self.quitting => {
                    self.want_visible = true;
                    self.raise = true;
                    self.retry_at = None;
                    if self.worker.is_some() {
                        // The callback already queued the move; avoid an extra restore.
                        self.pending = Some(Move::Show);
                    }
                    if self.worker.is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
                        self.hidden = false;
                        context.send_viewport_cmd(ViewportCommand::Visible(true));
                    }
                }
                Event::Quit => {
                    self.quitting = true;
                    context.send_viewport_cmd(ViewportCommand::Close);
                }
                Event::Moved { action, hidden, error } => {
                    if self.pending == Some(action) {
                        self.pending = None;
                    }
                    self.hidden = hidden;
                    if let Some(error) = error {
                        eprintln!("Rap workspace move failed: {error}");
                        if action == Move::Hide {
                            self.want_visible = true;
                        }
                        self.retry_at = Some(Instant::now() + Duration::from_secs(1));
                    } else {
                        self.retry_at = None;
                        if action == Move::Show {
                            self.raise = false;
                        }
                    }
                }
                Event::Show => {}
            }
        }
        if self.hidden && !self.active() {
            self.want_visible = true;
        }
        if !self.quitting && self.active() && context.input(|i| i.viewport().close_requested()) {
            if self.worker.is_some() {
                context.send_viewport_cmd(ViewportCommand::CancelClose);
                self.want_visible = false;
                self.raise = false;
            } else if std::env::var_os("WAYLAND_DISPLAY").is_none() {
                context.send_viewport_cmd(ViewportCommand::CancelClose);
                context.send_viewport_cmd(ViewportCommand::Visible(false));
                self.hidden = true;
            } // Other Wayland compositors cannot hide via Visible(false): close normally.
        }
        if self.quitting {
            return;
        }
        if let Some(worker) = &self.worker {
            if self.pending.is_none() {
                if let Some(deadline) = self.retry_at
                    && let Some(remaining) = deadline.checked_duration_since(Instant::now())
                {
                    context.request_repaint_after(remaining);
                    return;
                }
                let action = if self.want_visible && (self.hidden || self.raise) {
                    Some(Move::Show)
                } else if !self.want_visible && !self.hidden && self.active() {
                    Some(Move::Hide)
                } else {
                    None
                };
                if let Some(action) = action {
                    if worker.send(action).is_ok() {
                        self.pending = Some(action);
                    } else {
                        eprintln!("Rap workspace worker stopped");
                        self.want_visible = true;
                        self.worker = None;
                    }
                }
            }
        } else if self.hidden && !self.active() {
            self.hidden = false;
            context.send_viewport_cmd(ViewportCommand::Visible(true));
        }
    }
}

impl Drop for TrayService {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            std::mem::drop(handle.shutdown()); // No blocking wait on the UI thread.
        }
    }
}

/// All compositor I/O stays on this worker, including recovery after watcher loss.
fn hyprland_worker(
    sender: &mpsc::Sender<Event>,
    context: &egui::Context,
    registered: &Arc<AtomicBool>,
    quitting: &Arc<AtomicBool>,
) -> Option<mpsc::Sender<Move>> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let instance = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    let socket = Path::new(&runtime).join("hypr").join(instance).join(".socket.sock");
    let (commands, incoming) = mpsc::channel();
    let sender = sender.clone();
    let context = context.clone();
    let registered = Arc::clone(registered);
    let quitting = Arc::clone(quitting);
    match std::thread::Builder::new()
        .name("rap-workspace".into())
        .spawn(move || {
            let pid = std::process::id();
            let mut hidden = false;
            // Keep this connection for the entire hidden interval. A restarted bus
            // cannot make the old ksni service reachable on this connection.
            let mut bus = None;
            loop {
                let action = if hidden {
                    match incoming.recv_timeout(Duration::from_secs(1)) {
                        Ok(action) => action,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            let reachable = if !registered.load(Ordering::Acquire) {
                                false
                            } else {
                                let check = if let Some(connection) = &bus {
                                    tray_reachable(connection)
                                } else {
                                    match zbus::blocking::Connection::session() {
                                        Ok(connection) => {
                                            let result = tray_reachable(&connection);
                                            bus = Some(connection);
                                            result
                                        }
                                        Err(error) => Err(error),
                                    }
                                };
                                check.unwrap_or(false)
                            };
                            if !reachable {
                                eprintln!("Rap tray registration lost; restoring window");
                                registered.store(false, Ordering::Release);
                                if let Err(error) = restore_window(&socket, pid) {
                                    eprintln!("Rap cannot restore window: {error}");
                                    if !recover_window(&socket, pid, &sender, &context, &quitting) {
                                        break;
                                    }
                                }
                                hidden = false;
                                bus = None;
                                if sender.send(Event::Show).is_err()
                                    || sender.send(Event::Moved {
                                        action: Move::Show, hidden: false, error: None,
                                    }).is_err()
                                {
                                    break;
                                }
                                context.request_repaint();
                            }
                            continue;
                        }
                    }
                } else {
                    match incoming.recv() {
                        Ok(action) => action,
                        Err(_) => break,
                    }
                };
                if quitting.load(Ordering::Acquire) && action == Move::Hide {
                    continue;
                }
                let result = match action {
                    Move::Hide if !registered.load(Ordering::Acquire) => Ok(false),
                    Move::Hide => {
                        let moved = move_window(&socket, "name:rap-hidden", pid);
                        if moved.is_ok() && registered.load(Ordering::Acquire) {
                            Ok(true)
                        } else {
                            // The command may have executed even if its reply was lost.
                            // Restore before claiming the window is still visible.
                            restore_window(&socket, pid).map(|()| false)
                        }
                    }
                    Move::Show => restore_window(&socket, pid).map(|()| false),
                };
                let (moved_hidden, error) = match result {
                    Ok(hidden) => (hidden, None),
                    Err(error) => {
                        eprintln!("Rap workspace move failed: {error}");
                        if !recover_window(&socket, pid, &sender, &context, &quitting) {
                            break;
                        }
                        (false, Some(error))
                    }
                };
                hidden = moved_hidden;
                if !hidden {
                    bus = None;
                }
                if sender.send(Event::Moved { action, hidden, error }).is_err() {
                    break;
                }
                context.request_repaint();
            }
        }) {
        Ok(_) => Some(commands),
        Err(err) => {
            eprintln!("Rap cannot start workspace worker: {err}");
            None
        }
    }
}

/// Probe the actual SNI registration, not ksni's handle channel (which can stay
/// open after the session bus disconnects). ksni's sole tray owns the first name
/// generated for this process; a watcher restart without that item is not a tray.
fn tray_reachable(connection: &zbus::blocking::Connection) -> zbus::Result<bool> {
    let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
    let bus = zbus::blocking::Proxy::new(
        connection, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
    )?;
    let owned: bool = bus.call("NameHasOwner", &(name.as_str(),))?;
    if !owned {
        return Ok(false);
    }
    // Build a fresh proxy for each check: a cached property could outlive the
    // watcher process and incorrectly report the old registration after restart.
    let watcher = zbus::blocking::Proxy::new(
        connection, "org.kde.StatusNotifierWatcher", "/StatusNotifierWatcher",
        "org.kde.StatusNotifierWatcher",
    )?;
    let items: Vec<String> = watcher.get_property("RegisteredStatusNotifierItems")?;
    let host: bool = watcher.get_property("IsStatusNotifierHostRegistered")?;
    let item_path = format!("{name}/StatusNotifierItem");
    Ok(host && items.iter().any(|item| item == &name || item == &item_path))
}

/// A failed reply can mean the move already happened. Recover offscreen without UI frames.
fn recover_window(
    socket: &Path,
    pid: u32,
    sender: &mpsc::Sender<Event>,
    context: &egui::Context,
    quitting: &AtomicBool,
) -> bool {
    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(300));
        if restore_window(socket, pid).is_ok() {
            return true;
        }
    }
    eprintln!("Rap cannot restore window; closing instead of leaving playback hidden");
    quitting.store(true, Ordering::Release);
    let _ = sender.send(Event::Quit);
    context.request_repaint();
    if let Err(error) = close_window(socket, pid) {
        eprintln!("Rap cannot close window via compositor: {error}");
        std::process::exit(1);
    }
    false
}

fn ask(socket: &Path, command: &str) -> Result<String, String> {
    let mut stream = UnixStream::connect(socket).map_err(|err| err.to_string())?;
    stream.set_read_timeout(Some(Duration::from_millis(700))).map_err(|err| err.to_string())?;
    stream.set_write_timeout(Some(Duration::from_millis(700))).map_err(|err| err.to_string())?;
    stream.write_all(command.as_bytes()).map_err(|err| err.to_string())?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).map_err(|err| err.to_string())?;
    Ok(reply)
}

fn dispatch(socket: &Path, command: &str) -> Result<(), String> {
    let reply = ask(socket, command)?;
    if reply.trim() == "ok" {
        Ok(())
    } else {
        Err(format!("{command}: {reply}"))
    }
}

fn move_window(socket: &Path, workspace: &str, pid: u32) -> Result<(), String> {
    let workspace = serde_json::to_string(workspace).map_err(|err| err.to_string())?;
    dispatch(socket, &format!(
        "eval hl.dispatch(hl.dsp.window.move({{ window = \"pid:{pid}\", workspace = {workspace}, follow = false }}))"
    ))
}

fn close_window(socket: &Path, pid: u32) -> Result<(), String> {
    dispatch(socket, &format!(
        "eval hl.dispatch(hl.dsp.window.close({{ window = \"pid:{pid}\" }}))"
    ))
}

fn restore_window(socket: &Path, pid: u32) -> Result<(), String> {
    let reply = ask(socket, "j/activeworkspace")?;
    let workspace: serde_json::Value = serde_json::from_str(&reply).map_err(|err| err.to_string())?;
    let name = workspace["name"].as_str().ok_or("Active workspace has no name")?;
    if name == "rap-hidden" {
        return Err("Inactive workspace is currently active".into());
    }
    let destination = if let Some(id) = workspace["id"].as_i64().filter(|id| *id > 0) {
        id.to_string()
    } else {
        format!("name:{name}")
    };
    move_window(socket, &destination, pid)?;
    // Focusing is separate: failure cannot undo the already successful restore.
    let focus = format!("eval hl.dispatch(hl.dsp.focus({{ window = \"pid:{pid}\" }}))");
    if let Err(err) = dispatch(socket, &focus)
        && let Err(fallback) = dispatch(socket, &format!("dispatch focuswindow pid:{pid}"))
    {
        eprintln!("Rap cannot focus restored window: {err}; {fallback}");
    }
    Ok(())
}
