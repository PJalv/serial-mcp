use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyModifiers,
        MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color as TColor, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Terminal,
};
use serde_json::{json, Value};
use std::{
    collections::{HashSet, VecDeque},
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::Shutdown,
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, PermissionsExt},
        net::UnixStream,
        process::CommandExt,
    },
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

const V: u64 = 2;
const MAX: usize = 4 * 1024 * 1024;
const SCROLLBACK: usize = 2000;
const MAX_DEVICE_TABS: usize = 16;
static STOP: AtomicBool = AtomicBool::new(false);
extern "C" fn stop_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    id: u64,
    events: VecDeque<Value>,
    overflow: bool,
}
impl Client {
    fn connect() -> Result<Self, String> {
        let uid = unsafe { libc::geteuid() };
        let dir = if let Ok(p) = std::env::var("SERIAL_MCP_RUNTIME_DIR") {
            PathBuf::from(p)
        } else if let Ok(p) = std::env::var("XDG_RUNTIME_DIR") {
            PathBuf::from(p).join("serial-mcp")
        } else {
            PathBuf::from(format!("/tmp/serial-mcp-{uid}"))
        };
        if !dir.is_absolute() {
            return Err("runtime directory must be absolute".into());
        }
        let mut b = fs::DirBuilder::new();
        b.mode(0o700);
        match b.create(&dir) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.to_string()),
        }
        let m = fs::symlink_metadata(&dir).map_err(|e| e.to_string())?;
        if !m.file_type().is_dir() || m.uid() != uid || m.permissions().mode() & 0o077 != 0 {
            return Err("unsafe daemon runtime directory".into());
        }
        let p = dir.join("daemon.sock");
        if p.as_os_str().as_encoded_bytes().len() >= 108 {
            return Err("daemon socket path is too long".into());
        }
        let stream = UnixStream::connect(p).map_err(|e| format!("daemon unavailable: {e}"))?;
        #[cfg(target_os = "linux")]
        {
            let mut c: libc::ucred = unsafe { std::mem::zeroed() };
            let mut n = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
            let r = unsafe {
                libc::getsockopt(
                    std::os::fd::AsRawFd::as_raw_fd(&stream),
                    libc::SOL_SOCKET,
                    libc::SO_PEERCRED,
                    &mut c as *mut _ as *mut _,
                    &mut n,
                )
            };
            if r != 0 || c.uid != uid {
                return Err("daemon peer UID mismatch".into());
            }
        }
        let reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        Ok(Self {
            stream,
            reader,
            id: 0,
            events: VecDeque::new(),
            overflow: false,
        })
    }
    fn close(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }

    fn leave_device(&mut self, device: &mut Device) -> Result<(), String> {
        self.req(json!({"op":"unsubscribe","port":device.port}))?;
        self.req(json!({"op":"leave","port":device.port}))?;
        device.joined = false;
        record_gap(device);
        Ok(())
    }

    fn subscribe_device(&mut self, device: &Device) -> Result<(), String> {
        self.req(json!({"op":"subscribe","port":device.port}))?;
        Ok(())
    }

    fn req(&mut self, mut f: Value) -> Result<Value, String> {
        self.id += 1;
        f["v"] = json!(V);
        f["id"] = json!(self.id);
        let mut d = serde_json::to_vec(&f).map_err(|e| e.to_string())?;
        if d.len() + 1 > MAX {
            return Err("oversized daemon request".into());
        }
        d.push(b'\n');
        self.stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .map_err(|e| e.to_string())?;
        self.stream.write_all(&d).map_err(|e| e.to_string())?;
        self.stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|e| e.to_string())?;
        let started = Instant::now();
        let mut frames = 0usize;
        loop {
            if started.elapsed() > Duration::from_secs(5) || frames >= 256 {
                return Err("daemon request exceeded bounded response/event budget; outcome unknown and not retried".into());
            }
            frames += 1;
            let mut l = Vec::new();
            (&mut self.reader)
                .take((MAX + 1) as u64)
                .read_until(b'\n', &mut l)
                .map_err(|e| e.to_string())?;
            if l.is_empty() || l.len() > MAX || l.last() != Some(&b'\n') {
                return Err("daemon closed or sent oversized frame".into());
            }
            let x: Value =
                serde_json::from_slice(&l).map_err(|_| "malformed daemon frame".to_string())?;
            if x.get("v").and_then(Value::as_u64) != Some(V) {
                return Err("unsupported daemon protocol".into());
            }
            if x.get("event").is_some() {
                if self.events.len() >= 256 {
                    self.events.pop_front();
                    self.overflow = true;
                }
                self.events.push_back(x);
                continue;
            }
            if x.get("id").and_then(Value::as_u64) == Some(self.id) {
                return if x["ok"] == true {
                    Ok(x["result"].clone())
                } else {
                    Err(x["error"]["message"]
                        .as_str()
                        .unwrap_or("daemon error")
                        .to_string())
                };
            }
        }
    }
}
#[derive(Default)]
struct Device {
    port: String,
    device_id: Option<String>,
    state: String,
    baud: String,
    control: String,
    screen: vt100::Parser,
    gap: bool,
    new_output: bool,
    selection: Option<((u16, u16), (u16, u16))>,
    selecting: bool,
    joined: bool,
    gap_acknowledged: bool,
    screen_width: u16,
    input_warning: Option<String>,
    paste_mode_uncertain: bool,
}
impl Device {
    fn new(port: String) -> Self {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        Self {
            port,
            device_id: None,
            state: "available".into(),
            baud: "—".into(),
            control: "available".into(),
            screen: vt100::Parser::new(
                rows.saturating_sub(4).max(1),
                vt_screen_width(cols),
                SCROLLBACK,
            ),
            gap: false,
            new_output: false,
            selection: None,
            selecting: false,
            joined: false,
            gap_acknowledged: false,
            screen_width: vt_screen_width(cols),
            input_warning: None,
            paste_mode_uncertain: false,
        }
    }
}
/// Release the membership of the device that is not the one being opened.
///
/// Returns false when the leave failed; the caller then stops, because the
/// daemon's answer is unknown and `active` must not keep naming a device the
/// TUI is no longer a member of. On success `active` is cleared before the new
/// join is attempted, so a failed join leaves a consistent sidebar-only state.
#[allow(clippy::too_many_arguments)]
fn release_previous_membership(
    c: &mut Client,
    devs: &mut [Device],
    active: &mut Option<usize>,
    keep: usize,
    daemon_lost: &mut bool,
    leave_outcome_unknown: &mut bool,
    operation_failure: &mut Option<String>,
    status: &mut String,
) -> bool {
    let Some(previous) = active.filter(|&previous| previous != keep) else {
        return true;
    };
    if let Err(error) = c.leave_device(&mut devs[previous]) {
        c.close();
        *daemon_lost = true;
        *leave_outcome_unknown = true;
        devs[previous].joined = false;
        record_gap(&mut devs[previous]);
        *active = None;
        let failure = leave_failure_status(&error);
        *operation_failure = Some(failure.clone());
        *status = failure;
        return false;
    }
    *active = None;
    true
}

/// The width the cached screen should track, or `None` to leave it alone.
///
/// The buffer must match the pane it is rendered into: vt100 stores cells and
/// wraps at the width it was given, so a wider buffer than the pane hides new
/// output past the right edge, and a narrower one clips text the pane could
/// show. The one case that must not resize is a narrow terminal whose pane is
/// hidden behind the full-width drawer: shrinking then would truncate cells
/// vt100 cannot restore, losing cached output with no gap to show for it.
fn layout_screen_width(term_width: u16, focus_pane: bool, pane_hidden_drawer: bool) -> Option<u16> {
    if pane_hidden_drawer && !focus_pane {
        None
    } else {
        Some(vt_screen_width(term_width))
    }
}

fn fit_cached_screen(device: &mut Device, rows: u16, width: u16) {
    device.screen_width = width.max(1);
    device
        .screen
        .screen_mut()
        .set_size(rows.max(1), device.screen_width);
}

fn record_gap(device: &mut Device) {
    device.gap = true;
    device.gap_acknowledged = false;
}

fn spawn_daemon() -> Result<Child, String> {
    Command::new("serial-mcp-daemon")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start daemon: {e}"))
}

trait SpawnedProcess {
    fn has_exited(&mut self) -> std::io::Result<bool>;
}

impl SpawnedProcess for Child {
    fn has_exited(&mut self) -> std::io::Result<bool> {
        self.try_wait().map(|status| status.is_some())
    }
}

fn connect_with_daemon_start<T>(
    connect: impl FnMut() -> Result<T, String>,
    spawn: impl FnMut() -> Result<Child, String>,
) -> Result<T, String> {
    let started = Instant::now();
    connect_with_daemon_start_policy(
        connect,
        spawn,
        || started.elapsed(),
        std::thread::sleep,
        Duration::from_secs(10),
        5,
    )
}

fn connect_with_daemon_start_policy<T, P: SpawnedProcess>(
    mut connect: impl FnMut() -> Result<T, String>,
    mut spawn: impl FnMut() -> Result<P, String>,
    mut elapsed: impl FnMut() -> Duration,
    mut sleep: impl FnMut(Duration),
    timeout: Duration,
    max_attempts: usize,
) -> Result<T, String> {
    let mut child: Option<P> = None;
    let mut attempts = 0;
    let mut last_error: String;
    loop {
        match connect() {
            Ok(client) => return Ok(client),
            Err(e) => last_error = e,
        }

        if let Some(process) = child.as_mut() {
            match process.has_exited() {
                Ok(true) => child = None,
                Ok(false) => {}
                Err(e) => {
                    last_error = format!("could not inspect daemon startup: {e}");
                    child = None;
                }
            }
        }
        if child.is_none() {
            if attempts >= max_attempts || elapsed() >= timeout {
                return Err(format!("daemon did not start: {last_error}"));
            }
            match spawn() {
                Ok(process) => child = Some(process),
                Err(e) => last_error = e,
            }
            attempts += 1;
        }
        if elapsed() >= timeout {
            return Err(format!("daemon did not start: {last_error}"));
        }
        sleep(Duration::from_millis(100));
    }
}

fn connect_or_start_daemon() -> Result<Client, String> {
    if let Ok(client) = Client::connect() {
        return Ok(client);
    }
    connect_with_daemon_start(Client::connect, spawn_daemon)
}
fn main() {
    if let Err(e) = run() {
        eprintln!("serial-mcp-terminal: {e}");
        std::process::exit(1)
    }
}
fn run() -> Result<(), String> {
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "--version" => {
                println!("serial-mcp-terminal {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                println!("serial-mcp-terminal — shared serial multi-device TUI\\nUsage: serial-mcp-terminal [--help|--version]\\nCtrl-] toggles pane/sidebar focus without leaving. Sidebar: Up/Down or j/k, Enter opens, a acknowledges a gap, d disconnects, q/Ctrl-Q quits. Pane: Esc leaves; Ctrl-V then Ctrl-]/Esc sends literally; Ctrl-B opens shared baud controls; Ctrl-End scrolls to bottom.");
                return Ok(());
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    let mut c = connect_or_start_daemon()?;
    let hello = c.req(
        json!({"op":"hello","role":"human","name":"rust-human-terminal","pid":std::process::id()}),
    )?;
    if hello.get("mcp_clients").and_then(Value::as_u64).is_none() {
        return Err("daemon does not provide MCP-only client counts; update daemon".into());
    }
    let mut devs: Vec<Device> = Vec::new();
    let mut active: Option<usize> = None;
    let mut focus_pane = true;
    let mut selected = 0usize;
    let mut count = hello["mcp_clients"].as_u64().unwrap_or(0).to_string();
    let mut status = "Ready".to_string();
    let mut baud_prompt: Option<String> = None;
    let mut baud_prompt_target: Option<String> = None;
    let mut baud_change_pending = false;
    let mut baud_confirm = false;
    let mut baud_size_warning = false;
    let mut leave_outcome_unknown = false;
    let mut operation_failure: Option<String> = None;
    let mut subscription_failed = false;
    let mut literal_next = false;
    let mut daemon_lost = false;
    unsafe {
        libc::signal(libc::SIGINT, stop_signal as libc::sighandler_t);
        libc::signal(libc::SIGTERM, stop_signal as libc::sighandler_t);
        libc::signal(libc::SIGHUP, stop_signal as libc::sighandler_t);
    }
    enable_raw_mode().map_err(|e| e.to_string())?;
    let mut out = std::io::stdout();
    if let Err(e) = execute!(
        out,
        EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        EnableBracketedPaste
    ) {
        let _ = execute!(
            out,
            crossterm::event::DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
        return Err(e.to_string());
    }
    let mut result = (|| -> Result<(), String> {
        let mut term = Terminal::new(CrosstermBackend::new(out)).map_err(|e| e.to_string())?;
        loop {
            if STOP.load(Ordering::SeqCst) {
                break;
            }
            if let Some(i) = active {
                let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
                if let Some(screen_width) = layout_screen_width(width, focus_pane, width < 64) {
                    fit_cached_screen(&mut devs[i], height.saturating_sub(4).max(1), screen_width);
                }
            }
            let preserve_failure = leave_outcome_unknown || subscription_failed;
            let mut snapshot_ports = HashSet::new();
            let snapshot = c.req(json!({"op":"status"})).ok();
            let snapshot_ok = snapshot
                .as_ref()
                .is_some_and(|response| response["devices"].is_array());
            if let Some(s) = snapshot.as_ref() {
                count = s["mcp_clients"]
                    .as_u64()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".into());
                if count == "?" && !preserve_failure {
                    status = "MCP-only client count unavailable".into();
                }
                snapshot_ports = s["devices"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|d| d["port"].as_str().map(str::to_owned))
                    .collect();
            } else {
                daemon_lost = true;
                c.close();
                baud_confirm = false;
                baud_prompt = None;
                baud_prompt_target = None;
                baud_change_pending = false;
                baud_size_warning = false;
                if let Some(i) = active.take() {
                    devs[i].joined = false;
                    record_gap(&mut devs[i]);
                }
                if !preserve_failure {
                    status = "Daemon unavailable · r reconnects · cached screens retained".into();
                }
            }
            if snapshot.is_some() && !snapshot_ok && !preserve_failure {
                status = "Device status incomplete · cached tabs retained".into();
            }
            let ports_result = if daemon_lost {
                None
            } else {
                c.req(json!({"op":"ports"}))
                    .ok()
                    .and_then(|x| x["result"].as_str().map(str::to_owned))
            };
            let ports_ok = ports_result.is_some();
            let ports = ports_result.unwrap_or_default();
            let listed: HashSet<String> = ports
                .lines()
                .filter_map(|l| l.split_whitespace().next())
                .filter(|p| p.starts_with('/'))
                .map(str::to_owned)
                .collect();
            if !daemon_lost && !ports_ok {
                status = "Port list unavailable · cached tabs retained".into();
            }
            let snapshots_complete = snapshot_ok && ports_ok && !daemon_lost;
            let tabs_ok = reconcile_device_tabs(
                &mut devs,
                if snapshot_ok && !daemon_lost {
                    snapshot.as_ref()
                } else {
                    None
                },
                (!daemon_lost && ports_ok).then_some(&listed),
                &mut active,
                &mut selected,
            );
            if !tabs_ok && !preserve_failure {
                status = "Device display limit reached; no inactive tab can be evicted".into();
            }
            if snapshots_complete {
                for d in &mut devs {
                    if !snapshot_ports.contains(&d.port)
                        && matches!(d.state.as_str(), "connected" | "closing")
                    {
                        d.state = if listed.contains(&d.port) {
                            "available"
                        } else {
                            "disconnected"
                        }
                        .into();
                        d.baud = "—".into();
                        d.control = "available".into();
                    }
                    if d.state == "available" && !listed.contains(&d.port) {
                        d.state = "disconnected".into();
                    }
                    if d.state == "disconnected" {
                        d.paste_mode_uncertain = false;
                        d.input_warning = None;
                    }
                }
            }
            if c.overflow {
                c.overflow = false;
                if let Some(i) = active {
                    record_gap(&mut devs[i]);
                }
                if !preserve_failure {
                    status =
                        "Daemon event queue overflow — output/settings may be incomplete".into();
                }
            }
            while let Some(ev) = c.events.pop_front() {
                match ev["event"].as_str().unwrap_or("") {
                    "shutdown" => {
                        daemon_lost = true;
                        baud_confirm = false;
                        baud_prompt = None;
                        baud_prompt_target = None;
                        baud_change_pending = false;
                        baud_size_warning = false;
                        if let Some(i) = active.take() {
                            devs[i].joined = false;
                            record_gap(&mut devs[i]);
                        }
                        if !preserve_failure {
                            status = "Daemon shutting down · cached screens retained".into();
                        }
                    }
                    "event_gap" => {
                        if let Some(i) = active {
                            record_gap(&mut devs[i]);
                        }
                        status = "Daemon event gap · state may be incomplete".into();
                    }
                    "settings_gap" => {
                        if let Some(i) = active {
                            record_gap(&mut devs[i]);
                        }
                        status = "Settings gap · current state refreshed from status".into();
                    }
                    _ => {}
                }
                if let Some((idx, d)) = devs.iter_mut().enumerate().find(|(_, d)| {
                    ev["device_id"]
                        .as_str()
                        .is_some_and(|id| d.device_id.as_deref() == Some(id))
                        || ev["port"].as_str().is_some_and(|port| d.port == port)
                }) {
                    match ev["event"].as_str().unwrap_or("") {
                        "output" if Some(idx) == active => {
                            if let Some(a) = ev["data"].as_str() {
                                if let Ok(bytes) = base64_decode(a) {
                                    d.screen.process(&bytes);
                                    if d.screen.screen().scrollback() > 0 {
                                        d.new_output = true;
                                    }
                                }
                            }
                        }
                        "output_gap" if Some(idx) == active => {
                            record_gap(d);
                        }
                        "device" if ev["state"] == "disconnected" => {
                            if Some(idx) == active
                                || baud_prompt_target.as_deref() == Some(d.port.as_str())
                            {
                                baud_confirm = false;
                                baud_prompt = None;
                                baud_prompt_target = None;
                                baud_change_pending = false;
                            }
                            d.state = "disconnected".into();
                            d.paste_mode_uncertain = false;
                            d.input_warning = None;
                            d.joined = false;
                            record_gap(d);
                            let mut leave_failed = false;
                            if Some(idx) == active {
                                if let Err(error) = c.leave_device(d) {
                                    c.close();
                                    daemon_lost = true;
                                    leave_outcome_unknown = true;
                                    let failure = leave_failure_status(&error);
                                    operation_failure = Some(failure.clone());
                                    status = failure;
                                    leave_failed = true;
                                }
                                active = None;
                                selected = idx;
                            }
                            if !leave_failed {
                                status =
                                    "Device disconnected · select and press Enter to reconnect"
                                        .into();
                            }
                        }
                        "settings" => {
                            d.baud = ev["baudrate"]
                                .as_u64()
                                .map(|n| n.to_string())
                                .unwrap_or(d.baud.clone());
                        }
                        _ => {}
                    }
                }
            }
            if let Some(i) = active {
                if !devs[i].joined {
                    match c.subscribe_device(&devs[i]) {
                        Ok(()) => {
                            devs[i].joined = true;
                            subscription_failed = false;
                        }
                        Err(error) => {
                            c.close();
                            daemon_lost = true;
                            subscription_failed = true;
                            record_gap(&mut devs[i]);
                            active = None;
                            let failure = subscription_failure_status(&error);
                            operation_failure = Some(failure.clone());
                            status = failure;
                        }
                    }
                }
            }
            if let Some(failure) = operation_failure.as_ref() {
                status = failure.clone();
            }
            let (term_width, term_height) = crossterm::terminal::size().unwrap_or((80, 24));
            let narrow = term_width < 64;
            let pane_visible = active.is_some() && focus_pane;
            let sidebar_width = if narrow {
                if pane_visible {
                    0
                } else {
                    term_width.min(26)
                }
            } else {
                28
            };
            let gap_device = if active.is_some() && focus_pane {
                active.filter(|&i| devs[i].gap)
            } else {
                devs.get(selected)
                    .filter(|device| device.gap)
                    .map(|_| selected)
            };
            let gap_header_rows = term_height.saturating_sub(3).max(1);
            let warning_device = active.or(gap_device).or_else(|| {
                (!devs.is_empty()).then_some(selected.min(devs.len().saturating_sub(1)))
            });
            let input_warning = warning_device.and_then(|i| devs[i].input_warning.as_ref());
            let paste_mode_uncertain = warning_device.is_some_and(|i| devs[i].paste_mode_uncertain);
            let mut display_status = input_warning.map_or_else(
                || status.clone(),
                |warning| {
                    if status.starts_with(warning) {
                        status.clone()
                    } else {
                        format!("{warning} · {status}")
                    }
                },
            );
            if leave_outcome_unknown
                && !display_status
                    .to_ascii_lowercase()
                    .contains("leave outcome unknown")
            {
                display_status.push_str(" · Leave outcome unknown; r reconnects");
            }
            if subscription_failed
                && !display_status
                    .to_ascii_lowercase()
                    .contains("subscription failed")
            {
                display_status.push_str(" · Output subscription failed; r reconnects, then rejoin");
            }
            if daemon_lost && !display_status.to_ascii_lowercase().contains("r reconnects") {
                display_status.push_str(" · Daemon unavailable; r reconnects");
            }
            if gap_device.is_some_and(|i| devs[i].state == "disconnected")
                && !display_status
                    .to_ascii_lowercase()
                    .contains("device disconnected")
            {
                display_status
                    .push_str(" · Device disconnected; select and press Enter to reconnect");
            }
            if paste_mode_uncertain
                && !display_status
                    .to_ascii_lowercase()
                    .contains("application may remain in paste mode")
            {
                display_status.push_str(
                    " · Paste mode outcome unknown; application may remain in paste mode",
                );
            }
            let recovery_input_blocked =
                daemon_lost || leave_outcome_unknown || subscription_failed;
            let prompt_header = if recovery_input_blocked {
                Some(gap_device.map_or_else(
                    || recovery_prompt_header(&display_status, term_width, gap_header_rows),
                    |i| {
                        gap_prompt_header_for(
                            &devs[i],
                            &count,
                            &display_status,
                            term_width,
                            gap_header_rows,
                            false,
                        )
                    },
                ))
            } else if baud_confirm {
                let candidates = [
                    format!(" serial-mcp · MCP clients: {count}\n Shared baud affects all clients. Confirm y/n"),
                    format!("MCP{count}\nShared baud affects all\nConfirm y/n"),
                    "Shared baud? y/n".to_owned(),
                    "y/n".to_owned(),
                ];
                Some(
                    fitting_prompt(&candidates, term_width, term_height)
                        .unwrap_or_else(|| "Resize".to_owned()),
                )
            } else if let Some(baud) = baud_prompt.as_ref() {
                let candidates = [
                        format!(" serial-mcp · MCP clients: {count}\n Baud rate 1–4000000: {baud} · Enter confirms · Esc cancels"),
                        format!("MCP{count}\nBaud:{baud}\nRange 1-4000000\nEnter=apply\nEsc=cancel"),
                        format!("Baud:{baud}\n1-4000000\nEnter apply/Esc cancel"),
                        format!("Baud {baud} Enter apply/Esc cancel"),
                        format!("{baud} Enter apply/Esc cancel"),
                    ];
                Some(
                    fitting_prompt(&candidates, term_width, term_height)
                        .unwrap_or_else(|| "Resize".to_owned()),
                )
            } else if input_warning.is_some() && gap_device.is_none() {
                Some(input_warning_header(
                    &display_status,
                    term_width,
                    gap_header_rows,
                    None,
                ))
            } else if baud_size_warning {
                Some(if term_width < 7 {
                    "Resize".to_owned()
                } else if term_width < 20 {
                    "Resize terminal".to_owned()
                } else {
                    "Resize terminal to change baud".to_owned()
                })
            } else {
                gap_device.map(|i| {
                    gap_prompt_header_for(
                        &devs[i],
                        &count,
                        &display_status,
                        term_width,
                        gap_header_rows,
                        !focus_pane && !recovery_input_blocked,
                    )
                })
            };
            let gap_header_visible = !baud_confirm
                && baud_prompt.is_none()
                && !baud_size_warning
                && (gap_device.is_some() || input_warning.is_some());
            let header_height = prompt_header
                .as_ref()
                .map(|header| {
                    let rows = prompt_header_height(header, term_width, term_height);
                    if gap_header_visible {
                        rows.min(gap_header_rows)
                    } else {
                        rows
                    }
                })
                .unwrap_or(2);
            let pane_content_top = header_height.saturating_add(1);
            let screen_rows = term_height.saturating_sub(4).max(1);
            let pane_height = term_height
                .saturating_sub(header_height)
                .saturating_sub(2)
                .min(screen_rows);
            let visible_devices = pane_height as usize;
            let screen_row_offset = screen_rows.saturating_sub(pane_height);
            let sidebar_offset = selected.saturating_add(1).saturating_sub(visible_devices);
            if let Some(i) = active {
                if let Some(screen_width) = layout_screen_width(term_width, focus_pane, narrow) {
                    fit_cached_screen(&mut devs[i], screen_rows, screen_width);
                }
            }
            term.draw(|f| {
                let size = f.area();
                let rows = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Length(header_height), Constraint::Min(0)])
                    .split(size);
                let status_device = active.or_else(|| {
                    if devs.is_empty() {
                        None
                    } else {
                        Some(selected.min(devs.len() - 1))
                    }
                });
                let device_status = status_device
                    .map(|i| {
                        let marker = if devs[i].gap {
                            "OUTPUT GAP"
                        } else if devs[i].gap_acknowledged {
                            "gap acknowledged; lost output not recovered"
                        } else if devs[i].new_output {
                            "NEW OUTPUT BELOW"
                        } else {
                            ""
                        };
                        format!(
                            "{} · {} baud · control {} {}",
                            devs[i].state, devs[i].baud, devs[i].control, marker
                        )
                    })
                    .unwrap_or_default();
                let is_prompt = prompt_header.is_some();
                let header = prompt_header.unwrap_or_else(|| {
                    format!(
                        " serial-mcp · MCP clients: {count} · {}    {}    {}",
                        if focus_pane && active.is_some() {
                            "focus:pane"
                        } else {
                            "focus:sidebar"
                        },
                        status,
                        device_status
                    )
                });
                let header = Paragraph::new(header).wrap(Wrap { trim: true });
                if is_prompt {
                    f.render_widget(header, rows[0]);
                } else {
                    f.render_widget(
                        header.block(Block::default().borders(Borders::BOTTOM)),
                        rows[0],
                    );
                }
                let cols = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Length(sidebar_width), Constraint::Min(0)])
                    .split(rows[1]);
                let items: Vec<ListItem> = devs
                    .iter()
                    .enumerate()
                    .skip(sidebar_offset)
                    .take(visible_devices)
                    .map(|(i, d)| {
                        ListItem::new(format!(
                            "{}{} {}  {} · {} baud · control {}{}",
                            if Some(i) == active {
                                "●"
                            } else if i == selected {
                                "›"
                            } else {
                                " "
                            },
                            if d.gap { " GAP" } else { "" },
                            d.port,
                            d.state,
                            d.baud,
                            d.control,
                            if d.joined { " · joined" } else { "" }
                        ))
                    })
                    .collect();
                f.render_widget(
                    List::new(items).block(
                        Block::default()
                            .title(if focus_pane && active.is_some() {
                                " Devices · a ack · Ctrl-] "
                            } else {
                                " Devices · FOCUS · a ack · d del "
                            })
                            .borders(Borders::ALL),
                    ),
                    cols[0],
                );
                let lines = active
                    .map(|i| {
                        let screen = devs[i].screen.screen();
                        let row_offset = screen.size().0.saturating_sub(pane_height);
                        (0..screen.size().0.min(pane_height))
                            .map(|visible_row| {
                                let r = visible_row.saturating_add(row_offset);
                                let spans = (0..screen.size().1)
                                    .filter_map(|c| {
                                        let cell = screen.cell(r, c)?;
                                        if cell.is_wide_continuation() {
                                            return None;
                                        }
                                        let content = if cell.has_contents() {
                                            cell.contents()
                                        } else {
                                            " "
                                        };
                                        let mut modifier = Modifier::empty();
                                        if cell.bold() {
                                            modifier |= Modifier::BOLD;
                                        }
                                        if cell.italic() {
                                            modifier |= Modifier::ITALIC;
                                        }
                                        if cell.underline() {
                                            modifier |= Modifier::UNDERLINED;
                                        }
                                        if cell.inverse() {
                                            modifier |= Modifier::REVERSED;
                                        }
                                        if devs[i].selection.is_some_and(|((sy, sx), (ey, ex))| {
                                            let (lo, hi) = if (sy, sx) <= (ey, ex) {
                                                ((sy, sx), (ey, ex))
                                            } else {
                                                ((ey, ex), (sy, sx))
                                            };
                                            (r, c) >= lo && (r, c) <= hi
                                        }) {
                                            modifier |= Modifier::REVERSED;
                                        }
                                        Some(Span::styled(
                                            content.to_owned(),
                                            Style::default()
                                                .fg(to_color(cell.fgcolor()))
                                                .bg(to_color(cell.bgcolor()))
                                                .add_modifier(modifier),
                                        ))
                                    })
                                    .collect::<Vec<_>>();
                                Line::from(spans)
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_else(|| vec![Line::from("Choose a device · Enter joins · q quits")]);
                if pane_height > 0 {
                    f.render_widget(
                        Paragraph::new(lines).block(
                            Block::default()
                                .title(if active.is_some() {
                                    if focus_pane {
                                        " Terminal · FOCUS · Ctrl-] sidebar · Ctrl-B baud · Ctrl-End bottom "
                                    } else {
                                        " Terminal · unfocused · Enter on the joined device to return "
                                    }
                                } else {
                                    " Serial terminal "
                                })
                                .borders(Borders::ALL),
                        ),
                        cols[1],
                    );
                }
                if let Some(i) = active {
                    let screen = devs[i].screen.screen();
                    let row_offset = screen.size().0.saturating_sub(pane_height);
                    if pane_height > 0
                        && !screen.hide_cursor()
                        && screen.scrollback() == 0
                        && screen.cursor_position().0 >= row_offset
                    {
                        let (y, x) = screen.cursor_position();
                        f.set_cursor_position((
                            cols[1].x.saturating_add(1 + x),
                            cols[1].y.saturating_add(1 + y - row_offset),
                        ));
                    }
                }
            })
            .map_err(|e| e.to_string())?;
            if event::poll(Duration::from_millis(80)).map_err(|e| e.to_string())? {
                match event::read().map_err(|e| e.to_string())? {
                    Event::Resize(cols, rows) => {
                        if !baud_dialog_supported(cols, rows)
                            && (baud_confirm || baud_prompt.is_some())
                        {
                            baud_confirm = false;
                            baud_prompt = None;
                            baud_prompt_target = None;
                            baud_change_pending = false;
                            baud_size_warning = true;
                            status = "Resize terminal to change baud".into();
                        } else if baud_size_warning && baud_dialog_supported(cols, rows) {
                            baud_size_warning = false;
                            status = "Ready".into();
                        }
                        if let Some(i) = active {
                            if let Some(screen_width) =
                                layout_screen_width(cols, focus_pane, cols < 64)
                            {
                                fit_cached_screen(
                                    &mut devs[i],
                                    rows.saturating_sub(4).max(1),
                                    screen_width,
                                );
                            }
                        }
                    }
                    Event::Key(k) if k.code == KeyCode::Char('q') && !(active.is_some() && focus_pane) => break,
                    Event::Key(k) if k.code==KeyCode::Char('r') && daemon_lost => {
                        match connect_or_start_daemon(){Ok(mut new_client)=>match new_client.req(json!({"op":"hello","role":"human","name":"rust-human-terminal","pid":std::process::id()})){Ok(h) if h["mcp_clients"].as_u64().is_some()=>{if let Some(i)=active.take(){devs[i].joined=false;devs[i].gap=true;}count=h["mcp_clients"].as_u64().unwrap_or(0).to_string();c=new_client;daemon_lost=false;leave_outcome_unknown=false;subscription_failed=false;operation_failure=None;status="Reconnected · select device explicitly".into();},_=>status="Daemon reconnect failed".into()},Err(_)=>status="Daemon reconnect failed".into()}
                    }
                    Event::Key(k)
                        if ((k.code == KeyCode::Char(']')
                            && k.modifiers.contains(KeyModifiers::CONTROL))
                            || (k.code == KeyCode::Char('5')
                                && k.modifiers.contains(KeyModifiers::CONTROL))
                            || k.code == KeyCode::Char('\x1d'))
                            && !literal_next =>
                    {
                        baud_prompt=None;baud_prompt_target=None;baud_change_pending=false;baud_confirm=false;baud_size_warning=false;
                        literal_next = false;
                        if let Some(i) = active {
                            if focus_pane {
                                focus_pane = false;
                                selected = i;
                                status = "Sidebar focus · Enter reopens pane · a acknowledges gap".into();
                            } else {
                                focus_pane = true;
                                status = "Pane focus · Ctrl-] returns to sidebar".into();
                            }
                        } else {
                            focus_pane = false;
                            status = "Sidebar".into();
                        }
                    }
                    Event::Key(k) if baud_confirm => {
                        baud_confirm = false;
                        let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
                        if !baud_dialog_supported(width, height) {
                            baud_size_warning = true;
                            status = "Resize terminal to change baud".into();
                        } else if k.code == KeyCode::Char('y') {
                            baud_size_warning = false;
                            baud_change_pending = true;
                            baud_prompt_target = active.map(|i| devs[i].port.clone());
                            baud_prompt = Some(String::new());
                            status = "Enter shared baud rate".into();
                        } else {
                            status = "Baud change cancelled".into();
                        }
                    }
                    Event::Key(k) if baud_prompt.is_some() => {
                        let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
                        if !baud_dialog_supported(width, height) {
                            baud_prompt = None;
                            baud_prompt_target = None;
                            baud_change_pending = false;
                            baud_size_warning = true;
                            status = "Resize terminal to change baud".into();
                            continue;
                        }
                        match k.code {
                            KeyCode::Char(c) if c.is_ascii_digit() => {
                                let baud = baud_prompt.as_mut().unwrap();
                                if !push_baud_digit(baud, c) {
                                    baud_prompt = None;
                                    baud_prompt_target = None;
                                    baud_change_pending = false;
                                    status = "Baud entry is limited to 7 digits".into();
                                }
                            }
                            KeyCode::Char(_) => { baud_prompt=None; baud_prompt_target=None; baud_change_pending=false; status="Invalid baud rate; digits only".into(); }
                            KeyCode::Backspace => {
                                baud_prompt.as_mut().unwrap().pop();
                            }
                            KeyCode::Esc => {
                                baud_prompt = None;
                                baud_prompt_target = None;
                                baud_change_pending=false;
                                status = "Baud entry cancelled".into();
                            }
                            KeyCode::Enter => {
                                let Some(i) = baud_prompt_target.take().and_then(|port| {
                                    devs.iter().position(|device| device.port == port)
                                }) else {
                                    baud_prompt = None;
                                    baud_change_pending = false;
                                    status = "Baud target is no longer available".into();
                                    continue;
                                };
                                let changing=std::mem::take(&mut baud_change_pending);
                                let raw = baud_prompt.take().unwrap();
                                match raw.parse::<u32>() {
                                    Ok(baud) if (1..=4_000_000).contains(&baud) => {
                                        if changing {
                                            match c.req(json!({"op":"human_set_baudrate","port":devs[i].port,"baudrate":baud})) {
                                                Ok(result)=>{devs[i].baud=result["baudrate"].as_u64().map(|v|v.to_string()).unwrap_or(baud.to_string());status="Shared baud changed; other participants notified".into();},
                                                Err(e)=>status=format!("Baud change failed: {e}")
                                            }
                                        } else {
                                            if !release_previous_membership(
                                                &mut c,
                                                &mut devs,
                                                &mut active,
                                                i,
                                                &mut daemon_lost,
                                                &mut leave_outcome_unknown,
                                                &mut operation_failure,
                                                &mut status,
                                            ) {
                                                continue;
                                            }
                                            match c.req(json!({"op":"join","port":devs[i].port,"baudrate":baud})) {
                                                Ok(joined) => {
                                                    devs[i].device_id=joined["device_id"].as_str().map(str::to_owned);
                                                    devs[i].baud=joined["baudrate"].as_u64().map(|v|v.to_string()).unwrap_or(devs[i].baud.clone());
                                                    devs[i].state="connected".into();
                                                    if let Err(error) = c.subscribe_device(&devs[i]) {
                                                        c.close();
                                                        daemon_lost = true;
                                                        subscription_failed = true;
                                                        record_gap(&mut devs[i]);
                                                        devs[i].joined = false;
                                                        let failure = subscription_failure_status(&error);
                                                        operation_failure = Some(failure.clone());
                                                        status = failure;
                                                    } else {
                                                        devs[i].joined=true;
                                                        active=Some(i);
                                                        focus_pane=true;
                                                        baud_size_warning=false;
                                                        leave_outcome_unknown=false;
                                                        subscription_failed=false;
                                                        literal_next=false;
                                                        status="Connected · Ctrl-] toggles focus".into();
                                                    }
                                                },
                                                Err(e) => status=format!("Join failed: {e}")
                                            }
                                        }
                                    }
                                    _ => {
                                        status = "Invalid baud rate; enter 1 through 4000000".into()
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Event::Key(_) if daemon_lost || leave_outcome_unknown || subscription_failed => {}
                    Event::Key(k) if !focus_pane || active.is_none() => match k.code {
                        KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                        KeyCode::Down | KeyCode::Char('j') => { selected = (selected + 1).min(devs.len().saturating_sub(1)) }
                        KeyCode::Char('a') if !devs.is_empty() => {
                            let i = selected;
                            if devs[i].gap {
                                devs[i].gap = false;
                                devs[i].gap_acknowledged = true;
                                status = "Gap acknowledged · cached output kept · lost output is not recovered".into();
                            } else {
                                status = "No gap to acknowledge".into();
                            }
                        }
                        KeyCode::Char('d') if !devs.is_empty() => {
                            let i = selected;
                            if devs[i].joined || Some(i) == active {
                                match c.leave_device(&mut devs[i]) {
                                    Ok(()) => {
                                        if Some(i) == active {
                                            active = None;
                                            focus_pane = false;
                                        }
                                        status = "Disconnected from selected device".into();
                                    }
                                    Err(error) => {
                                        c.close();
                                        daemon_lost = true;
                                        leave_outcome_unknown = true;
                                        devs[i].joined = false;
                                        record_gap(&mut devs[i]);
                                        let failure = leave_failure_status(&error);
                                        operation_failure = Some(failure.clone());
                                        status = failure;
                                    }
                                }
                            } else {
                                status = "Not joined to selected device".into();
                            }
                        }
                        KeyCode::Enter if !devs.is_empty() => {
                            let i = selected;
                            if devs[i].joined && Some(i) == active {
                                focus_pane = true;
                                literal_next = false;
                                status = "Pane focus · Ctrl-] returns to sidebar".into();
                            } else if devs[i].state == "connected" {
                                if !release_previous_membership(
                                    &mut c,
                                    &mut devs,
                                    &mut active,
                                    i,
                                    &mut daemon_lost,
                                    &mut leave_outcome_unknown,
                                    &mut operation_failure,
                                    &mut status,
                                ) {
                                    continue;
                                }
                                match c.req(json!({"op":"join","port":devs[i].port})) {
                                    Ok(joined) => {
                                        if let Err(error) = c.subscribe_device(&devs[i]) {
                                            c.close();
                                            daemon_lost = true;
                                            subscription_failed = true;
                                            record_gap(&mut devs[i]);
                                            devs[i].joined = false;
                                            let failure = subscription_failure_status(&error);
                                            operation_failure = Some(failure.clone());
                                            status = failure;
                                        } else {
                                            devs[i].device_id=joined["device_id"].as_str().map(str::to_owned);
                                            devs[i].joined = true;
                                            active = Some(i);
                                            focus_pane = true;
                                            baud_size_warning = false;
                                            leave_outcome_unknown = false;
                                            subscription_failed = false;
                                            operation_failure = None;
                                            literal_next = false;
                                            status = "Connected · Ctrl-] toggles focus".into();
                                        }
                                    }
                                    Err(e) => status = format!("Join failed: {e}"),
                                }
                            } else {
                                let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
                                if baud_dialog_supported(width, height) {
                                    baud_size_warning = false;
                                    baud_prompt_target = Some(devs[i].port.clone());
                                    baud_prompt = Some(String::new());
                                    status = "Enter baud rate".into();
                                } else {
                                    baud_size_warning = true;
                                    status = "Resize terminal to change baud".into();
                                }
                            }
                        }
                        _ => {}
                    },
                    Event::Key(k)
                        if active.is_some()
                            && focus_pane
                            && matches!(
                                route_pane_key(k, literal_next, false),
                                PaneKeyRoute::ScrollToBottom
                            ) => {
                        if let Some(i) = active {
                            devs[i].screen.screen_mut().set_scrollback(0);
                            devs[i].new_output = false;
                        }
                    }
                    Event::Key(k)
                        if active.is_some() && focus_pane && k.code == KeyCode::Esc && !literal_next =>
                    {
                        if let Some(i) = active.take() {
                            match c.leave_device(&mut devs[i]) {
                                Ok(()) => status = "Sidebar".into(),
                                Err(error) => {
                                    c.close();
                                    daemon_lost = true;
                                    leave_outcome_unknown = true;
                                    devs[i].joined = false;
                                    record_gap(&mut devs[i]);
                                    let failure = leave_failure_status(&error);
                                    operation_failure = Some(failure.clone());
                                    status = failure;
                                }
                            }
                        }
                        baud_size_warning = false;
                        literal_next = false;
                    }
                    Event::Key(k) if active.is_some() && focus_pane && !literal_next && (k.code==KeyCode::Char('b') || k.code==KeyCode::Char('B')) && k.modifiers.contains(KeyModifiers::CONTROL) => {
                        let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
                        if baud_dialog_supported(width, height) {
                            baud_size_warning = false;
                            baud_confirm = true;
                            status="Changing baud affects all participants and may disrupt active AI work. Confirm y/n".into();
                        } else {
                            baud_size_warning = true;
                            status="Resize terminal to change baud".into();
                        }
                    }
                    Event::Key(k) => {
                        if let Some(i) = active.filter(|_| focus_pane) {
                            let route = route_pane_key(
                                k,
                                literal_next,
                                devs[i].screen.screen().application_cursor(),
                            );
                            let bytes = match route {
                                PaneKeyRoute::ArmLiteral => {
                                    literal_next = true;
                                    status = "Next key sent literally".into();
                                    continue;
                                }
                                PaneKeyRoute::ScrollToBottom => {
                                    devs[i].screen.screen_mut().set_scrollback(0);
                                    devs[i].new_output = false;
                                    literal_next = false;
                                    continue;
                                }
                                PaneKeyRoute::Send(bytes) => bytes,
                            };
                            literal_next = false;
                            if !bytes.is_empty() {
                                if let Err(e) = c.req(json!({
                                    "op": "input",
                                    "port": devs[i].port,
                                    "data": base64_encode(&bytes)
                                })) {
                                    let warning = format!("Input outcome unknown: {e}; not replayed");
                                    devs[i].input_warning = Some(warning.clone());
                                    status = warning;
                                }
                            }
                        }
                    }
                    Event::Paste(_) if baud_confirm || baud_prompt.is_some() => {}
                    Event::Paste(text) if active.is_some() && focus_pane => {
                        if let Some(i) = active {
                            let bracketed = devs[i].screen.screen().bracketed_paste();
                            let mut failed = false;
                            if bracketed {
                                if let Err(error) = c.req(json!({
                                    "op": "input",
                                    "port": devs[i].port,
                                    "data": base64_encode(b"\x1b[200~")
                                })) {
                                    let close_result = c.req(json!({
                                        "op": "input",
                                        "port": devs[i].port,
                                        "data": base64_encode(b"\x1b[201~")
                                    }));
                                    let warning = match close_result {
                                        Ok(_) => {
                                            devs[i].paste_mode_uncertain = false;
                                            format!(
                                                "Paste opening delimiter outcome unknown ({error}); closing delimiter sent; body not sent"
                                            )
                                        },
                                        Err(_) => {
                                            devs[i].paste_mode_uncertain = true;
                                            format!(
                                                "Paste opening delimiter outcome unknown ({error}); closing delimiter outcome unknown; application may remain in paste mode"
                                            )
                                        }
                                    };
                                    devs[i].input_warning = Some(warning.clone());
                                    status = warning;
                                    failed = true;
                                }
                            }
                            if !failed {
                                for chunk in text.as_bytes().chunks(64 * 1024) {
                                    if let Err(error) = c.req(json!({
                                        "op": "input",
                                        "port": devs[i].port,
                                        "data": base64_encode(chunk)
                                    })) {
                                        let close_result = if bracketed {
                                            c.req(json!({
                                                "op": "input",
                                                "port": devs[i].port,
                                                "data": base64_encode(b"\x1b[201~")
                                            }))
                                        } else {
                                            Ok(json!({}))
                                        };
                                        let close_note = if bracketed {
                                            match close_result {
                                                Ok(_) => {
                                                    devs[i].paste_mode_uncertain = false;
                                                    "closing delimiter sent"
                                                },
                                                Err(_) => {
                                                    devs[i].paste_mode_uncertain = true;
                                                    "closing delimiter outcome unknown; application may remain in paste mode"
                                                }
                                            }
                                        } else {
                                            ""
                                        };
                                        let warning = format!(
                                            "Paste body chunk outcome unknown: {error}; remaining bytes were not sent; {close_note}"
                                        );
                                        devs[i].input_warning = Some(warning.clone());
                                        status = warning;
                                        failed = true;
                                        break;
                                    }
                                }
                            }
                            if bracketed && !failed {
                                match c.req(json!({
                                    "op": "input",
                                    "port": devs[i].port,
                                    "data": base64_encode(b"\x1b[201~")
                                })) {
                                    Ok(_) => {
                                        devs[i].paste_mode_uncertain = false;
                                        if let Some(warning) = devs[i].input_warning.as_mut() {
                                            if warning.contains("application may remain in paste mode") {
                                                let previous = warning.clone();
                                                let resolved = resolve_paste_mode_warning(warning);
                                                if status.starts_with(&previous) {
                                                    status = status.replacen(&previous, &resolved, 1);
                                                }
                                                *warning = resolved;
                                            }
                                        }
                                    }
                                    Err(error) => {
                                        devs[i].paste_mode_uncertain = true;
                                        let warning = format!(
                                            "Paste body sent; closing delimiter outcome unknown ({error}); application may remain in paste mode"
                                        );
                                        devs[i].input_warning = Some(warning.clone());
                                        status = warning;
                                    }
                                }
                            }
                        }
                    }
                    Event::Mouse(_) if baud_confirm || baud_prompt.is_some() => {}
                    Event::Mouse(m) if active.is_some() && focus_pane && pane_height > 0 && m.column>=sidebar_width && m.row>=pane_content_top && m.row<term_height.saturating_sub(1) && !m.modifiers.contains(KeyModifiers::SHIFT) && active.is_some_and(|i|devs[i].screen.screen().mouse_protocol_mode()!=vt100::MouseProtocolMode::None) => {
                        if let Some(i)=active {
                            let screen=devs[i].screen.screen();
                            let col=m.column.saturating_sub(sidebar_width).max(1);
                            let row = m
                                .row
                                .saturating_sub(header_height)
                                .saturating_add(screen_row_offset)
                                .max(1);
                            if let Some(bytes)=encode_mouse(screen.mouse_protocol_mode(),screen.mouse_protocol_encoding(),m.kind,m.modifiers,col,row) {
                                if let Err(e)=c.req(json!({"op":"input","port":devs[i].port,"data":base64_encode(&bytes)})){let warning=format!("Mouse input outcome unknown: {e}; not replayed");devs[i].input_warning=Some(warning.clone());status=warning;}
                            }
                        }
                    }
                    Event::Mouse(m) if active.is_some() && focus_pane && pane_height > 0 && (m.modifiers.contains(KeyModifiers::SHIFT) || active.is_some_and(|i|devs[i].screen.screen().mouse_protocol_mode()==vt100::MouseProtocolMode::None)) && m.kind==MouseEventKind::Down(crossterm::event::MouseButton::Left) && m.column>=sidebar_width && m.row>=pane_content_top && m.row<term_height.saturating_sub(1) => {
                        if let Some(i)=active { let pos=(m.row.saturating_sub(pane_content_top).saturating_add(screen_row_offset),m.column.saturating_sub(sidebar_width+1)); devs[i].selection=Some((pos,pos)); devs[i].selecting=true; }
                    }
                    Event::Mouse(m) if active.is_some() && focus_pane && pane_height > 0 && (m.modifiers.contains(KeyModifiers::SHIFT) || active.is_some_and(|i|devs[i].screen.screen().mouse_protocol_mode()==vt100::MouseProtocolMode::None)) && m.kind==MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                        if let Some(i)=active { if devs[i].selecting { if let Some((start,_))=devs[i].selection {let pos=(m.row.saturating_sub(pane_content_top).saturating_add(screen_row_offset),m.column.saturating_sub(sidebar_width+1));devs[i].selection=Some((start,pos));} } }
                    }
                    Event::Mouse(m) if active.is_some() && m.kind==MouseEventKind::Up(crossterm::event::MouseButton::Left) => {if let Some(i)=active{devs[i].selecting=false;}}
                    Event::Mouse(m) if active.is_some() && focus_pane && pane_height > 0 && m.column>=sidebar_width && m.kind == MouseEventKind::ScrollUp => {
                        if let Some(i) = active {
                            let s = devs[i].screen.screen_mut();
                            let n = s.scrollback();
                            s.set_scrollback(n.saturating_add(3));
                            devs[i].new_output = true;
                        }
                    }
                    Event::Mouse(m) if active.is_some() && focus_pane && pane_height > 0 && m.column>=sidebar_width && m.kind == MouseEventKind::ScrollDown => {
                        if let Some(i) = active {
                            let s = devs[i].screen.screen_mut();
                            let n = s.scrollback();
                            s.set_scrollback(n.saturating_sub(3));
                            if n <= 3 {
                                devs[i].new_output = false;
                            }
                        }
                    }
                    Event::Mouse(m)
                        if m.kind == MouseEventKind::Down(crossterm::event::MouseButton::Left) =>
                    {
                        let target = sidebar_hit_target(
                            m.row,
                            pane_content_top,
                            sidebar_offset,
                            visible_devices,
                            devs.len(),
                        );
                        if let Some(target) = target.filter(|_| {
                            m.column < sidebar_width && m.row < term_height.saturating_sub(1)
                        }) {
                            selected = target;
                            focus_pane = false;
                            literal_next = false;
                            baud_size_warning = false;
                            status = "Device selected · Enter joins".into();
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    })();
    if let Some(i) = active {
        if let Err(error) = c.leave_device(&mut devs[i]) {
            c.close();
            result = Err(format!(
                "could not leave device on exit; daemon connection closed for cleanup: {error}"
            ));
        }
    }
    let mut out = std::io::stdout();
    let _ = execute!(
        out,
        crossterm::event::DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    let _ = c.req(json!({"op":"bye"}));
    result
}
fn add_device(
    devices: &mut Vec<Device>,
    port: &str,
    active: &mut Option<usize>,
    selected: &mut usize,
    protected: &HashSet<String>,
) -> bool {
    if devices.iter().any(|device| device.port == port) {
        return true;
    }
    if devices.len() >= MAX_DEVICE_TABS {
        let Some(evicted) = devices.iter().enumerate().find_map(|(index, device)| {
            (device.state == "disconnected"
                && !protected.contains(&device.port)
                && Some(index) != *active
                && index != *selected)
                .then_some(index)
        }) else {
            return false;
        };
        devices.remove(evicted);
        if *selected > evicted {
            *selected -= 1;
        }
        if let Some(index) = active.as_mut() {
            if *index > evicted {
                *index -= 1;
            }
        }
    }
    devices.push(Device::new(port.to_owned()));
    true
}

fn reconcile_listed_ports(
    devices: &mut Vec<Device>,
    listed: &HashSet<String>,
    protected: &HashSet<String>,
    active: &mut Option<usize>,
    selected: &mut usize,
) -> bool {
    for device in devices.iter_mut() {
        if listed.contains(&device.port) && device.state == "disconnected" {
            device.state = "available".into();
            device.baud = "—".into();
            device.control = "available".into();
        }
    }
    listed
        .iter()
        .all(|port| add_device(devices, port, active, selected, protected))
}

fn reconcile_device_tabs(
    devices: &mut Vec<Device>,
    snapshot: Option<&Value>,
    listed: Option<&HashSet<String>>,
    active: &mut Option<usize>,
    selected: &mut usize,
) -> bool {
    let Some(snapshot) = snapshot else {
        return true;
    };
    let Some(status_devices) = snapshot["devices"].as_array() else {
        return true;
    };
    let Some(listed) = listed else {
        return sync_devices(
            devices,
            snapshot,
            active,
            selected,
            &HashSet::new(),
            true,
            false,
        );
    };
    let mut protected = status_devices
        .iter()
        .filter_map(|device| device["port"].as_str().map(str::to_owned))
        .collect::<HashSet<_>>();
    protected.extend(listed.iter().cloned());
    let snapshot_tabs_ok =
        sync_devices(devices, snapshot, active, selected, &protected, true, true);
    let listed_tabs_ok = reconcile_listed_ports(devices, listed, &protected, active, selected);
    snapshot_tabs_ok && listed_tabs_ok
}

fn sync_devices(
    ds: &mut Vec<Device>,
    s: &Value,
    active: &mut Option<usize>,
    selected: &mut usize,
    protected: &HashSet<String>,
    allow_new_tabs: bool,
    allow_eviction: bool,
) -> bool {
    let mut fits = true;
    if let Some(a) = s["devices"].as_array() {
        for x in a {
            let p = x["port"].as_str().unwrap_or("");
            if p.is_empty() {
                continue;
            }
            let i = if let Some(i) = ds.iter().position(|device| device.port == p) {
                i
            } else if allow_new_tabs
                && (ds.len() < MAX_DEVICE_TABS || allow_eviction)
                && add_device(ds, p, active, selected, protected)
            {
                ds.len() - 1
            } else {
                if allow_new_tabs {
                    fits = false;
                }
                continue;
            };
            ds[i].device_id = x["device_id"].as_str().map(str::to_owned);
            ds[i].state = x["state"].as_str().unwrap_or("disconnected").into();
            ds[i].baud = x["baudrate"]
                .as_u64()
                .map(|x| x.to_string())
                .unwrap_or_else(|| "—".into());
            ds[i].control = x["control"].as_str().unwrap_or("available").into();
        }
    }
    fits
}
fn encode_mouse(
    mode: vt100::MouseProtocolMode,
    encoding: vt100::MouseProtocolEncoding,
    kind: MouseEventKind,
    modifiers: KeyModifiers,
    col: u16,
    row: u16,
) -> Option<Vec<u8>> {
    use crossterm::event::MouseButton;
    let (button, release) = match kind {
        MouseEventKind::Down(MouseButton::Left) => (Some(0), false),
        MouseEventKind::Down(MouseButton::Middle) => (Some(1), false),
        MouseEventKind::Down(MouseButton::Right) => (Some(2), false),
        MouseEventKind::Up(MouseButton::Left) => (Some(0), true),
        MouseEventKind::Up(MouseButton::Middle) => (Some(1), true),
        MouseEventKind::Up(MouseButton::Right) => (Some(2), true),
        MouseEventKind::Drag(MouseButton::Left)
            if matches!(
                mode,
                vt100::MouseProtocolMode::ButtonMotion | vt100::MouseProtocolMode::AnyMotion
            ) =>
        {
            (Some(32), false)
        }
        MouseEventKind::Drag(MouseButton::Middle)
            if matches!(
                mode,
                vt100::MouseProtocolMode::ButtonMotion | vt100::MouseProtocolMode::AnyMotion
            ) =>
        {
            (Some(33), false)
        }
        MouseEventKind::Drag(MouseButton::Right)
            if matches!(
                mode,
                vt100::MouseProtocolMode::ButtonMotion | vt100::MouseProtocolMode::AnyMotion
            ) =>
        {
            (Some(34), false)
        }
        MouseEventKind::Moved if mode == vt100::MouseProtocolMode::AnyMotion => (Some(35), false),
        MouseEventKind::ScrollUp => (Some(64), false),
        MouseEventKind::ScrollDown => (Some(65), false),
        _ => (None, false),
    };
    let mut code = button?;
    if mode == vt100::MouseProtocolMode::None
        || (mode == vt100::MouseProtocolMode::Press && release)
    {
        return None;
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    if encoding == vt100::MouseProtocolEncoding::Sgr {
        return Some(
            format!(
                "\x1b[<{code};{col};{row}{}",
                if release { 'm' } else { 'M' }
            )
            .into_bytes(),
        );
    }
    if release {
        code = (code & !0x03) | 3;
    }
    if encoding == vt100::MouseProtocolEncoding::Utf8 && (col > 2015 || row > 2015) {
        return None;
    }
    let (x, y) = if encoding == vt100::MouseProtocolEncoding::Utf8 {
        (col as u32 + 32, row as u32 + 32)
    } else {
        (col.min(223) as u32 + 32, row.min(223) as u32 + 32)
    };
    let mut out = vec![0x1b, b'[', b'M'];
    if encoding == vt100::MouseProtocolEncoding::Default {
        out.extend([code as u8 + 32, x as u8, y as u8]);
    } else {
        for n in [code as u32 + 32, x, y] {
            if let Some(c) = char::from_u32(n) {
                let mut buf = [0; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    Some(out)
}
fn wrapped_rows(text: &str, width: u16) -> u16 {
    Paragraph::new(text)
        .wrap(Wrap { trim: true })
        .line_count(width.max(1))
        .min(usize::from(u16::MAX)) as u16
}

fn prompt_header_height(text: &str, width: u16, height: u16) -> u16 {
    wrapped_rows(text, width).min(height.max(1))
}

fn leave_failure_status(error: &str) -> String {
    format!("Leave outcome unknown; connection closed for cleanup; r reconnects: {error}")
}

fn subscription_failure_status(error: &str) -> String {
    format!("Output subscription failed; r reconnects, then rejoin: {error}")
}

fn recovery_prompt_header(status: &str, width: u16, available_rows: u16) -> String {
    if status_indicates_input_uncertainty(status) {
        return input_warning_header(status, width, available_rows, None);
    }
    let lower = status.to_ascii_lowercase();
    let leaving = lower.contains("leave outcome unknown");
    let subscription = lower.contains("subscription");
    let reconnect = if width < 12 { "r retry" } else { "r reconnect" };

    if available_rows <= 1 {
        return if leaving && width >= 8 {
            "Leave? r".to_owned()
        } else if subscription && width >= 8 {
            "Sub? r".to_owned()
        } else if width >= 8 {
            "D lost r".to_owned()
        } else {
            reconnect.to_owned()
        };
    }

    let full = if leaving {
        "Leave outcome unknown · connection closed for cleanup · r reconnects"
    } else if subscription {
        "Output subscription failed; r reconnects, then rejoin"
    } else {
        "Daemon unavailable · r reconnects"
    };
    if available_rows > 2 && wrapped_rows(full, width) <= available_rows {
        return full.into();
    }

    let label = if leaving {
        if width < 16 {
            "Leave?"
        } else {
            "Leave uncertain"
        }
    } else if subscription {
        if width < 12 {
            "Sub?"
        } else if width < 16 {
            "Sub failed"
        } else {
            "Subscribe failed"
        }
    } else if width < 12 {
        "D lost"
    } else if width < 18 {
        "Daemon lost"
    } else {
        "Daemon unavailable"
    };
    format!("{label}\n{reconnect}")
}

fn resolve_paste_mode_warning(warning: &str) -> String {
    if let Some((prefix, _)) = warning.split_once("; closing delimiter outcome unknown") {
        format!("{prefix}; a closing delimiter was later sent")
    } else if let Some((prefix, _)) = warning.split_once("; application may remain in paste mode") {
        format!("{prefix}; a closing delimiter was later sent")
    } else {
        warning.to_owned()
    }
}

fn status_indicates_input_uncertainty(status: &str) -> bool {
    let lower = status.to_ascii_lowercase();
    lower.contains("input outcome unknown")
        || (lower.contains("paste") && lower.contains("outcome unknown"))
}

fn input_warning_header(
    status: &str,
    width: u16,
    available_rows: u16,
    count: Option<&str>,
) -> String {
    let lower = status.to_ascii_lowercase();
    let recovery = lower.contains("r reconnects")
        || lower.contains("daemon unavailable")
        || lower.contains("daemon shutting down")
        || lower.contains("subscription failed");
    let paste = lower.contains("paste") && lower.contains("outcome unknown");
    let paste_open = paste && lower.contains("opening delimiter");
    let paste_body =
        paste && (lower.contains("body chunk") || lower.contains("remaining bytes were not sent"));
    let paste_close = paste && lower.contains("closing delimiter outcome unknown");
    let paste_mode_uncertain =
        paste_close || lower.contains("application may remain in paste mode");
    let mouse = lower.contains("mouse input outcome unknown");
    let leaving = lower.contains("leave outcome unknown");
    let disconnected = lower.contains("device disconnected");
    let join_failed = lower.contains("join failed");
    let unavailable = lower.contains("unavailable");
    let other_failure = lower.contains("failed") || lower.contains("incomplete");
    let no_replay =
        lower.contains("not replayed") || lower.contains("remaining bytes were not sent");

    if available_rows >= 3 && wrapped_rows(status, width) <= available_rows {
        return status.to_owned();
    }

    let warning = if paste_body {
        if width >= 8 {
            "Partial?"
        } else {
            "P?part"
        }
    } else if paste_open {
        if width >= 13 {
            "Open? unknown"
        } else {
            "Open?"
        }
    } else if paste_close {
        if width >= 9 {
            "Body sent"
        } else {
            "Sent?"
        }
    } else if paste_mode_uncertain && !paste_open && !paste_body {
        if lower.contains("input outcome unknown") {
            if width >= 14 {
                "Input? unknown"
            } else {
                "Input?"
            }
        } else if width >= 11 {
            "Paste mode?"
        } else {
            "Mode?"
        }
    } else if paste {
        if width >= 9 {
            "Paste?"
        } else {
            "P?"
        }
    } else if mouse {
        if width >= 14 {
            "Mouse? unknown"
        } else {
            "Mouse?"
        }
    } else if width >= 14 {
        "Input? unknown"
    } else {
        "Unknown"
    };

    if available_rows <= 1 {
        let line = if paste_mode_uncertain {
            if paste_body {
                if recovery {
                    if width >= 13 {
                        "Part? NR M? r"
                    } else if width >= 9 {
                        "P? NR M?r"
                    } else {
                        "NR M?r"
                    }
                } else if disconnected {
                    if width >= 13 {
                        "Part? NR M? E"
                    } else {
                        "NR M? E"
                    }
                } else if leaving {
                    if width >= 16 {
                        "Part? NR M? L? r"
                    } else {
                        "NR M? r"
                    }
                } else if width >= 8 {
                    "P? NR M?"
                } else {
                    "NR M?"
                }
            } else if paste_open {
                if recovery {
                    if width >= 10 {
                        "Open? M? r"
                    } else {
                        "O? M? r"
                    }
                } else if disconnected {
                    if width >= 10 {
                        "Open? M? E"
                    } else {
                        "O? M? E"
                    }
                } else if leaving {
                    if width >= 13 {
                        "Open? M? L? r"
                    } else {
                        "O? M? r"
                    }
                } else if width >= 8 {
                    "Open? M?"
                } else {
                    "O? M?"
                }
            } else if paste_close {
                if recovery {
                    if width >= 10 {
                        "Sent? M? r"
                    } else {
                        "S? M? r"
                    }
                } else if disconnected {
                    if width >= 10 {
                        "Sent? M? E"
                    } else {
                        "S? M? E"
                    }
                } else if leaving {
                    if width >= 13 {
                        "Sent? M? L? r"
                    } else {
                        "S? M? r"
                    }
                } else if width >= 16 {
                    "Body sent; mode?"
                } else if width >= 8 {
                    "Sent? M?"
                } else {
                    "S? M?"
                }
            } else if lower.contains("not replayed") {
                if recovery {
                    if width >= 9 {
                        "I? NR M?r"
                    } else {
                        "NR M?r"
                    }
                } else if disconnected {
                    if width >= 10 {
                        "I? NR M? E"
                    } else {
                        "NR M? E"
                    }
                } else if leaving {
                    if width >= 11 {
                        "I? NR M? L? r"
                    } else {
                        "NR M? r"
                    }
                } else if width >= 10 {
                    "No rep; M?"
                } else {
                    "NR M?"
                }
            } else if recovery {
                "Mode? r"
            } else if disconnected {
                if width >= 11 {
                    "Mode? Enter"
                } else {
                    "M? Enter"
                }
            } else if leaving {
                if width >= 10 {
                    "Mode? L? r"
                } else {
                    "M? r"
                }
            } else if width >= 11 {
                "Paste mode?"
            } else {
                "Mode?"
            }
        } else if leaving && recovery {
            if paste_body && paste_mode_uncertain {
                if width >= 13 {
                    "Part? M? L? r"
                } else {
                    "P? M? r"
                }
            } else if paste_open && paste_mode_uncertain {
                if width >= 13 {
                    "Open? M? L? r"
                } else {
                    "O? M? r"
                }
            } else if paste_close {
                if width >= 13 {
                    "Sent? M? L? r"
                } else if width >= 10 {
                    "S? M? L? r"
                } else {
                    "S? M? r"
                }
            } else if paste {
                if width >= 12 {
                    "P? L? r"
                } else {
                    "P? r"
                }
            } else if mouse {
                if width >= 12 {
                    "M? L? r"
                } else {
                    "M? r"
                }
            } else if width >= 12 {
                "I? L? r"
            } else {
                "I? r"
            }
        } else if recovery {
            if paste_body && paste_mode_uncertain {
                if width >= 11 {
                    "Part? M? r"
                } else {
                    "P? M? r"
                }
            } else if paste_open && paste_mode_uncertain {
                if width >= 11 {
                    "Open? M? r"
                } else {
                    "O? M? r"
                }
            } else if paste_body {
                if width >= 9 {
                    "Part? r"
                } else {
                    "P? r"
                }
            } else if paste_close {
                if width >= 11 {
                    "Sent? M? r"
                } else {
                    "S? M? r"
                }
            } else if paste_open {
                if width >= 9 {
                    "Open? r"
                } else {
                    "O? r"
                }
            } else if paste {
                if width >= 9 {
                    "Paste? r"
                } else {
                    "P? r"
                }
            } else if mouse {
                if width >= 9 {
                    "Mouse? r"
                } else {
                    "M? r"
                }
            } else if width >= 11 {
                "Input? r"
            } else {
                "I? r"
            }
        } else if leaving {
            if lower.contains("not replayed") {
                if width >= 15 {
                    "Input? Leave?"
                } else {
                    "I? L?"
                }
            } else if width >= 15 {
                "Leave?/input?"
            } else {
                "Leave?"
            }
        } else if paste_body && paste_mode_uncertain && disconnected {
            if width >= 11 {
                "Part? M? E"
            } else {
                "P? M? E"
            }
        } else if paste_open && paste_mode_uncertain && disconnected {
            if width >= 11 {
                "Open? M? E"
            } else {
                "O? M? E"
            }
        } else if paste_close && disconnected {
            if width >= 13 {
                "Sent? Mode? E"
            } else if width >= 11 {
                "Sent? M? E"
            } else {
                "S? M? E"
            }
        } else if paste_body && disconnected {
            if width >= 11 {
                "Part? Enter"
            } else {
                "P? E"
            }
        } else if paste_open && disconnected {
            if width >= 11 {
                "Open? Enter"
            } else {
                "O? E"
            }
        } else if lower.contains("not replayed") && disconnected {
            if width >= 16 {
                "I? no rep; Enter"
            } else if width >= 10 {
                "I?; Enter"
            } else {
                "I? E"
            }
        } else if lower.contains("not replayed") {
            if width >= 16 {
                "Input? no replay"
            } else if width >= 9 {
                "I? no dup"
            } else {
                "I? dup?"
            }
        } else if disconnected {
            if width >= 12 {
                "Select+Enter"
            } else {
                "Enter"
            }
        } else if join_failed {
            if width >= 12 {
                "Join? input?"
            } else {
                "Join fail"
            }
        } else if lower.contains("port list unavailable") {
            if width >= 14 {
                "Ports? input?"
            } else {
                "Ports?"
            }
        } else if unavailable || other_failure {
            if width >= 13 {
                "Fail + input?"
            } else {
                "Fail?"
            }
        } else {
            warning
        };
        return line.chars().take(width.max(1) as usize).collect();
    }

    if recovery
        || leaving
        || disconnected
        || join_failed
        || unavailable
        || other_failure
        || lower.contains("not replayed")
        || paste_open
        || paste_close
        || paste_body
        || paste_mode_uncertain
    {
        let combined = if let Some(count) = count {
            format!("{warning} MCP{count}")
        } else {
            warning.to_owned()
        };
        let first = if combined.chars().count() <= width as usize {
            combined
        } else {
            warning.to_owned()
        };
        let current_line = if paste_body && paste_mode_uncertain {
            if recovery {
                if no_replay && width >= 13 {
                    "Rest? NR M? r"
                } else if no_replay {
                    "NR M?r"
                } else if width >= 10 {
                    "Rest? M? r"
                } else {
                    "M? r"
                }
            } else if disconnected {
                if no_replay && width >= 13 {
                    "Rest? NR M? E"
                } else if no_replay {
                    "NR M? E"
                } else if width >= 14 {
                    "Rest? M? Enter"
                } else {
                    "M? Enter"
                }
            } else if leaving {
                if no_replay {
                    "NR M? r"
                } else if width >= 15 {
                    "Unsent M? L? r"
                } else {
                    "M? L? r"
                }
            } else if no_replay {
                if width >= 9 {
                    "Rest? NR M?"
                } else {
                    "NR M?"
                }
            } else if width >= 14 {
                "Rest unsent M?"
            } else if width >= 9 {
                "Unsent M?"
            } else {
                "Unsnt M?"
            }
        } else if paste_open && paste_mode_uncertain {
            if recovery {
                if width >= 13 {
                    "No body; M? r"
                } else {
                    "M? r"
                }
            } else if disconnected {
                if width >= 17 {
                    "No body; M? Enter"
                } else {
                    "M? Enter"
                }
            } else if leaving {
                if width >= 14 {
                    "NoBody M? L? r"
                } else {
                    "M? L? r"
                }
            } else if width >= 9 {
                "M? noBody"
            } else {
                "M?Nbody"
            }
        } else if paste_close && paste_mode_uncertain {
            if recovery {
                if width >= 10 {
                    "Sent? M? r"
                } else {
                    "S? M? r"
                }
            } else if disconnected {
                if width >= 14 {
                    "Sent? M? Enter"
                } else {
                    "M? Enter"
                }
            } else if leaving {
                if width >= 13 {
                    "Sent? M? L? r"
                } else {
                    "M? L? r"
                }
            } else if width >= 11 {
                "Paste mode?"
            } else {
                "Mode?"
            }
        } else if paste_mode_uncertain && lower.contains("not replayed") {
            if recovery {
                if width >= 12 {
                    "No rep; M? r"
                } else if width >= 9 {
                    "NoRep M?r"
                } else {
                    "NR M?r"
                }
            } else if disconnected {
                if width >= 12 {
                    "No rep; M? E"
                } else {
                    "NR M? E"
                }
            } else if leaving {
                if width >= 15 {
                    "No rep; M? L? r"
                } else {
                    "NR M? r"
                }
            } else if width >= 13 {
                "No replay; M?"
            } else if width >= 9 {
                "NoRep M?"
            } else {
                "NR M?"
            }
        } else if paste_mode_uncertain && recovery {
            "Mode? r"
        } else if paste_mode_uncertain && disconnected {
            "M? Enter"
        } else if paste_mode_uncertain && leaving {
            if width >= 10 {
                "Mode? L? r"
            } else {
                "M? r"
            }
        } else if paste_mode_uncertain {
            "Paste mode?"
        } else if paste_body {
            if leaving {
                if width >= 16 {
                    "Rest unsent L? r"
                } else if width >= 12 {
                    "Rest? L? r"
                } else if width >= 9 {
                    "Rest L? r"
                } else {
                    "Rest r"
                }
            } else if disconnected {
                if width >= 12 {
                    "Rest; Enter"
                } else {
                    "Rest E"
                }
            } else if recovery {
                if width >= 13 {
                    "Rest? r retry"
                } else {
                    "Rest? r"
                }
            } else if width >= 11 {
                "Rest unsent"
            } else {
                "Rest?"
            }
        } else if paste_open {
            if recovery {
                if width >= 10 {
                    "No body r"
                } else {
                    "NoBody r"
                }
            } else if disconnected {
                if width >= 11 {
                    "No body; E"
                } else {
                    "NoBody E"
                }
            } else if leaving {
                if width >= 12 {
                    "No body; L? r"
                } else {
                    "NoBody r"
                }
            } else if width >= 13 {
                "Body not sent"
            } else if width >= 10 {
                "No body"
            } else {
                "NoBody"
            }
        } else if paste_close {
            if recovery {
                "Mode? r"
            } else if disconnected {
                if width >= 11 {
                    "Mode? Enter"
                } else {
                    "Mode? E"
                }
            } else if leaving {
                if width >= 10 {
                    "Mode? L? r"
                } else {
                    "Mode? r"
                }
            } else if width >= 11 {
                "Paste mode?"
            } else {
                "Mode?"
            }
        } else if lower.contains("not replayed") {
            if leaving {
                if width >= 15 {
                    "No replay; L? r"
                } else if width >= 12 {
                    "No dup; L? r"
                } else {
                    "No dup r"
                }
            } else if disconnected {
                if width >= 16 {
                    "No replay; Enter"
                } else if width >= 13 {
                    "No dup; Enter"
                } else {
                    "No dup E"
                }
            } else if recovery {
                if width >= 12 {
                    "No replay; r"
                } else {
                    "No dup r"
                }
            } else if width >= 9 {
                "No replay"
            } else {
                "No dup"
            }
        } else if leaving {
            "Leave? r"
        } else if recovery {
            if width < 12 {
                "r retry"
            } else {
                "r reconnect"
            }
        } else if disconnected {
            if width >= 15 {
                "Enter reconnect"
            } else if width >= 12 {
                "Select+Enter"
            } else {
                "Enter"
            }
        } else if join_failed {
            if width < 12 {
                "Join fail"
            } else {
                "Join failed"
            }
        } else if lower.contains("port list unavailable") {
            if width < 18 {
                "Ports?"
            } else {
                "Ports unavailable"
            }
        } else if unavailable {
            if width < 12 {
                "Offline"
            } else {
                "Unavailable"
            }
        } else if width < 16 {
            "Failed"
        } else {
            "Operation failed"
        };
        return format!("{first}\n{current_line}");
    }

    if let Some(count) = count {
        let with_count = format!("{warning} MCP{count}");
        if with_count.chars().count() <= width as usize {
            return with_count;
        }
    }
    warning.to_owned()
}

/// `ack_available` is false when `a` would not acknowledge right now: the pane
/// holds focus, so the key goes to the device, or recovery still blocks input.
fn gap_prompt_header_for(
    device: &Device,
    count: &str,
    status: &str,
    width: u16,
    available_rows: u16,
    ack_available: bool,
) -> String {
    if status_indicates_input_uncertainty(status) {
        return input_warning_header(status, width, available_rows, Some(count));
    }
    let lower_status = status.to_ascii_lowercase();
    let subscription_failure = lower_status.contains("subscription failed");
    let disconnected = lower_status.contains("device disconnected");
    let recovery = status.contains("r reconnects")
        || status.contains("Daemon unavailable")
        || status.contains("Daemon shutting down")
        || subscription_failure;
    let leave_uncertain = lower_status.contains("leave outcome unknown");
    let compact_status = if subscription_failure {
        if width < 12 {
            "Sub+r"
        } else {
            "Sub fail · r"
        }
    } else if leave_uncertain {
        "Leave? r"
    } else if recovery {
        if width < 12 {
            "r retry"
        } else {
            "r reconnect"
        }
    } else if status.starts_with("Join failed") {
        if width < 9 {
            "Join?"
        } else if width < 12 {
            "Join fail"
        } else {
            "Join failed"
        }
    } else if disconnected {
        if width >= 15 {
            "Enter reconnect"
        } else if width >= 12 {
            "Select+Enter"
        } else {
            "Enter"
        }
    } else if status.contains("unavailable") {
        if width < 12 {
            "Offline"
        } else {
            "Unavailable"
        }
    } else if status.contains("failed") {
        "Failed"
    } else if status.contains("incomplete") || status.contains("overflow") {
        "Incomplete"
    } else if status.starts_with("Connected") && device.joined {
        "Connected"
    } else if device.state == "disconnected" {
        if width < 12 {
            "Offline"
        } else {
            "Disconnected"
        }
    } else if !device.joined && device.state == "connected" {
        "Not joined"
    } else {
        "Available"
    };

    if available_rows == 1 {
        let one_line = if subscription_failure {
            format!("Sub+r MCP{count}")
        } else if leave_uncertain {
            "Leave? r".to_owned()
        } else if recovery {
            format!("r MCP{count}")
        } else if status.starts_with("Join failed")
            || status.starts_with("Device disconnected")
            || status.contains("unavailable")
        {
            compact_status.to_owned()
        } else if !ack_available {
            format!("GAP MCP{count}")
        } else if width >= 14 {
            format!("GAP a=ack MCP{count}")
        } else {
            "GAP a:ack".to_owned()
        };
        return one_line.chars().take(width.max(1) as usize).collect();
    }

    if width >= 64 && available_rows >= 3 {
        let ack = if ack_available {
            " · a acknowledges"
        } else {
            ""
        };
        let action = if recovery || leave_uncertain {
            format!(" · r reconnects{ack}")
        } else if disconnected {
            format!(" · select device, Enter reconnects{ack}")
        } else {
            ack.to_owned()
        };
        let full = format!(
            "OUTPUT GAP · MCP clients: {count} · {status} · {} · {} baud · control {}{action}",
            device.state, device.baud, device.control
        );
        if wrapped_rows(&full, width) <= available_rows {
            return full;
        }
    }

    // The acknowledgement action matters more than the baud here: a gap is
    // only cleared by an explicit key, so never trade the hint away for detail.
    let candidates = if ack_available {
        vec![
            format!("GAP a=ack MCP{count} {}", device.baud),
            format!("GAP a=ack MCP{count}"),
            "GAP a=ack".to_owned(),
        ]
    } else {
        vec![
            format!("GAP MCP{count} {}", device.baud),
            format!("GAP MCP{count}"),
            "GAP".to_owned(),
        ]
    };
    let first_line = candidates
        .iter()
        .find(|line| line.chars().count() <= width as usize)
        .cloned()
        .unwrap_or_else(|| "GAP".chars().take(width.max(1) as usize).collect());
    format!("{first_line}\n{compact_status}")
}

fn push_baud_digit(value: &mut String, digit: char) -> bool {
    if digit.is_ascii_digit() && value.len() < 7 {
        value.push(digit);
        true
    } else {
        false
    }
}

fn baud_dialog_supported(width: u16, height: u16) -> bool {
    width >= 16 && height >= 5
}

fn vt_screen_width(term_width: u16) -> u16 {
    let sidebar_width = if term_width < 64 { 0 } else { 28 };
    term_width
        .saturating_sub(sidebar_width)
        .saturating_sub(2)
        .max(1)
}

fn sidebar_hit_target(
    row: u16,
    pane_top: u16,
    sidebar_offset: usize,
    visible_rows: usize,
    device_count: usize,
) -> Option<usize> {
    let row_offset = usize::from(row.checked_sub(pane_top)?);
    if row_offset >= visible_rows {
        return None;
    }
    let target = sidebar_offset.saturating_add(row_offset);
    (target < device_count).then_some(target)
}

fn fitting_prompt(candidates: &[String], width: u16, height: u16) -> Option<String> {
    candidates
        .iter()
        .find(|candidate| wrapped_rows(candidate, width) <= height)
        .cloned()
}

fn to_color(color: vt100::Color) -> TColor {
    match color {
        vt100::Color::Default => TColor::Reset,
        vt100::Color::Idx(i) => TColor::Indexed(i),
        vt100::Color::Rgb(r, g, b) => TColor::Rgb(r, g, b),
    }
}
enum PaneKeyRoute {
    ArmLiteral,
    ScrollToBottom,
    Send(Vec<u8>),
}

fn route_pane_key(
    key: crossterm::event::KeyEvent,
    literal_next: bool,
    application_cursor: bool,
) -> PaneKeyRoute {
    if !literal_next
        && key.code == KeyCode::Char('v')
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        PaneKeyRoute::ArmLiteral
    } else if !literal_next
        && key.code == KeyCode::End
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        PaneKeyRoute::ScrollToBottom
    } else if literal_next
        && (key.code == KeyCode::Char(']')
            || key.code == KeyCode::Char('\x1d')
            || (key.code == KeyCode::Char('5') && key.modifiers.contains(KeyModifiers::CONTROL))
            || key.code == KeyCode::Esc)
    {
        PaneKeyRoute::Send(if key.code == KeyCode::Esc {
            vec![0x1b]
        } else {
            vec![0x1d]
        })
    } else {
        PaneKeyRoute::Send(key_bytes(key, application_cursor))
    }
}

fn key_bytes(k: crossterm::event::KeyEvent, application_cursor: bool) -> Vec<u8> {
    match k.code {
        KeyCode::Char(c) if k.modifiers.contains(KeyModifiers::CONTROL) => {
            let c = c.to_ascii_lowercase();
            let byte = if c.is_ascii_lowercase() {
                Some(c as u8 - b'a' + 1)
            } else {
                match c {
                    ' ' | '@' => Some(0),
                    '[' => Some(27),
                    '\\' => Some(28),
                    ']' => Some(29),
                    '^' => Some(30),
                    '_' => Some(31),
                    '?' => Some(127),
                    _ => None,
                }
            };
            byte.map(|v| vec![v]).unwrap_or_default()
        }
        KeyCode::Char(c) => {
            let mut b = [0; 4];
            let mut bytes = c.encode_utf8(&mut b).as_bytes().to_vec();
            if k.modifiers.contains(KeyModifiers::ALT) {
                bytes.insert(0, 27);
            }
            bytes
        }
        KeyCode::Enter => vec![13],
        KeyCode::Backspace => vec![127],
        KeyCode::Tab => vec![9],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Esc => vec![27],
        KeyCode::Up => {
            if application_cursor {
                b"\x1bOA".to_vec()
            } else {
                b"\x1b[A".to_vec()
            }
        }
        KeyCode::Down => {
            if application_cursor {
                b"\x1bOB".to_vec()
            } else {
                b"\x1b[B".to_vec()
            }
        }
        KeyCode::Right => {
            if application_cursor {
                b"\x1bOC".to_vec()
            } else {
                b"\x1b[C".to_vec()
            }
        }
        KeyCode::Left => {
            if application_cursor {
                b"\x1bOD".to_vec()
            } else {
                b"\x1b[D".to_vec()
            }
        }
        KeyCode::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => Vec::new(),
        },
        KeyCode::Null => vec![0],
        _ => Vec::new(),
    }
}
fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|_| "invalid daemon output encoding".into())
}
fn base64_encode(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Most header tests describe the normal case: `a` is available.
    fn gap_prompt_header(
        device: &Device,
        count: &str,
        status: &str,
        width: u16,
        available_rows: u16,
    ) -> String {
        gap_prompt_header_for(device, count, status, width, available_rows, true)
    }

    fn test_device(port: String) -> Device {
        let mut device = Device::new(port);
        device.screen.screen_mut().set_size(20, 80);
        device
    }

    #[test]
    fn acknowledgement_hint_only_appears_when_the_key_would_acknowledge() {
        let device = Device {
            port: "synthetic-device".into(),
            state: "connected".into(),
            baud: "115200".into(),
            control: "available".into(),
            ..Device::default()
        };
        let gap_status = "Output gap; cached screen retained";
        // Sidebar focused and nothing blocking: `a` works, so it is advertised.
        assert!(gap_prompt_header_for(&device, "3", gap_status, 80, 4, true).contains('a'));
        // Pane focused: `a` would be sent to the device, so never suggest it.
        for width in [8u16, 16, 32, 80] {
            for rows in [1u16, 2, 4] {
                let header = gap_prompt_header_for(&device, "3", gap_status, width, rows, false);
                assert!(
                    !header.contains("a=ack") && !header.contains("a acknowledges"),
                    "hint offered where the key would reach the device: {header:?}"
                );
            }
        }
        // Recovery blocks every key but the reconnect one.
        let recovery = "Daemon unavailable · r reconnects · cached screens retained";
        let header = gap_prompt_header_for(&device, "3", recovery, 80, 4, false);
        assert!(!header.contains("a acknowledges"), "{header:?}");
        assert!(header.contains("r reconnect"), "{header:?}");
    }

    #[test]
    fn the_cached_screen_tracks_the_pane_but_not_a_hidden_drawer() {
        // Widening must follow the pane, or new output past the pane edge is
        // stored but never shown; narrowing while the pane is visible is also
        // correct, because that is the width the user is reading at.
        assert_eq!(layout_screen_width(50, true, true), Some(48));
        assert_eq!(layout_screen_width(64, false, false), Some(34));
        assert_eq!(layout_screen_width(100, false, false), Some(70));
        // A narrow terminal hides the pane behind the drawer: shrinking then
        // would truncate cells vt100 cannot restore, with no gap to show it.
        assert_eq!(layout_screen_width(50, false, true), None);

        let mut device = test_device("synthetic-device".into());
        device.screen_width = 1;
        fit_cached_screen(&mut device, 20, 48);
        assert_eq!(device.screen.screen().size(), (20, 48));
        fit_cached_screen(&mut device, 24, 70);
        assert_eq!(device.screen.screen().size(), (24, 70));

        // Output written at the pane width is visible in the pane width.
        device.screen.process(b"RIGHTMARGIN_xxxxxxxxxxxxxxxxxxxx");
        let screen = device.screen.screen();
        let row: String = (0..70)
            .filter_map(|c| screen.cell(0, c).map(|cell| cell.contents().to_string()))
            .collect();
        assert!(row.contains("xxxxxxxxxxxxxxxxxxxx"), "{row:?}");
    }

    #[test]
    fn failed_previous_leave_clears_active_without_claiming_membership() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        let mut client = Client {
            stream,
            reader,
            id: 0,
            events: VecDeque::new(),
            overflow: false,
        };
        let server = std::thread::spawn(move || {
            let mut peer_reader = BufReader::new(peer.try_clone().unwrap());
            let mut line = String::new();
            peer_reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let response = json!({"v": V, "id": request["id"], "ok": false,
                "error": {"message": "synthetic unsubscribe rejection"}});
            peer.write_all(format!("{response}\n").as_bytes()).unwrap();
        });
        let mut devs = vec![
            test_device("synthetic-first".into()),
            test_device("synthetic-second".into()),
        ];
        devs[0].joined = true;
        let mut active = Some(0usize);
        let mut daemon_lost = false;
        let mut leave_outcome_unknown = false;
        let mut operation_failure = None;
        let mut status = String::new();
        assert!(!release_previous_membership(
            &mut client,
            &mut devs,
            &mut active,
            1,
            &mut daemon_lost,
            &mut leave_outcome_unknown,
            &mut operation_failure,
            &mut status,
        ));
        assert!(
            active.is_none(),
            "unknown leave outcome kept a stale active"
        );
        assert!(!devs[0].joined);
        assert!(devs[0].gap);
        assert!(daemon_lost && leave_outcome_unknown);
        client.close();
        server.join().unwrap();
    }

    #[test]
    fn successful_previous_leave_clears_active_before_the_new_join() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        let mut client = Client {
            stream,
            reader,
            id: 0,
            events: VecDeque::new(),
            overflow: false,
        };
        let server = std::thread::spawn(move || {
            let mut peer_reader = BufReader::new(peer.try_clone().unwrap());
            for _ in 1..=2 {
                let mut line = String::new();
                peer_reader.read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let response = json!({"v": V, "id": request["id"], "ok": true, "result": {}});
                peer.write_all(format!("{response}\n").as_bytes()).unwrap();
            }
        });
        let mut devs = vec![
            test_device("synthetic-first".into()),
            test_device("synthetic-second".into()),
        ];
        devs[0].joined = true;
        let mut active = Some(0usize);
        let mut daemon_lost = false;
        let mut leave_outcome_unknown = false;
        let mut operation_failure = None;
        let mut status = String::new();
        assert!(release_previous_membership(
            &mut client,
            &mut devs,
            &mut active,
            1,
            &mut daemon_lost,
            &mut leave_outcome_unknown,
            &mut operation_failure,
            &mut status,
        ));
        assert!(active.is_none(), "active must be cleared before joining");
        assert!(!devs[0].joined && devs[0].gap);
        assert!(!devs[1].joined, "the new device is not joined yet");
        client.close();
        server.join().unwrap();
    }

    #[test]
    fn genuine_gaps_require_an_explicit_acknowledgement() {
        let mut device = test_device("synthetic-device".into());
        assert!(!device.gap);
        record_gap(&mut device);
        assert!(device.gap);
        assert!(!device.gap_acknowledged);
        // A later genuine gap invalidates an earlier acknowledgement.
        device.gap = false;
        device.gap_acknowledged = true;
        record_gap(&mut device);
        assert!(device.gap);
        assert!(!device.gap_acknowledged);
    }

    #[test]
    fn leaving_marks_a_gap_only_when_membership_was_really_released() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        let mut client = Client {
            stream,
            reader,
            id: 0,
            events: VecDeque::new(),
            overflow: false,
        };
        let server = std::thread::spawn(move || {
            let mut peer_reader = BufReader::new(peer.try_clone().unwrap());
            for expected_id in 1..=2 {
                let mut line = String::new();
                peer_reader.read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["id"], expected_id);
                let response = json!({"v": V, "id": expected_id, "ok": true, "result": {}});
                peer.write_all(format!("{response}\n").as_bytes()).unwrap();
            }
        });
        let mut device = test_device("synthetic-device".into());
        device.joined = true;
        client.leave_device(&mut device).unwrap();
        assert!(!device.joined);
        assert!(device.gap);
        client.close();
        server.join().unwrap();
    }

    #[test]
    fn leave_failure_is_reported_without_claiming_local_departure() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        let mut client = Client {
            stream,
            reader,
            id: 0,
            events: VecDeque::new(),
            overflow: false,
        };
        let server = std::thread::spawn(move || {
            let mut peer_reader = BufReader::new(peer.try_clone().unwrap());
            for (expected_id, ok) in [(1, true), (2, false)] {
                let mut line = String::new();
                peer_reader.read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["id"], expected_id);
                let response = if ok {
                    json!({"v": V, "id": expected_id, "ok": true, "result": {}})
                } else {
                    json!({"v": V, "id": expected_id, "ok": false, "error": {"message": "synthetic leave rejection"}})
                };
                peer.write_all(format!("{response}\n").as_bytes()).unwrap();
            }
        });
        let mut device = test_device("synthetic-device".into());
        device.joined = true;
        assert!(client.leave_device(&mut device).is_err());
        assert!(device.joined);
        assert!(!device.gap);
        client.close();
        server.join().unwrap();
    }

    #[test]
    fn terminal_escape_sequences_are_confined_to_the_vt_buffer() {
        let mut parser = vt100::Parser::new(24, 80, SCROLLBACK);
        parser.process(b"before\x1b[2J\x1b[Hafter");
        let screen = parser.screen();
        assert_eq!(screen.cell(0, 0).unwrap().contents(), "a");
        assert_eq!(screen.cell(0, 4).unwrap().contents(), "r");
    }

    #[test]
    fn serial_key_bytes_include_control_and_arrow_sequences() {
        let ctrl_c = crossterm::event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(key_bytes(ctrl_c, false), vec![3]);
        let arrow = crossterm::event::KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(key_bytes(arrow, false), b"\x1b[A");
        let end = crossterm::event::KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
        assert_eq!(key_bytes(end, false), b"\x1b[F");
        let ctrl_q = crossterm::event::KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert_eq!(key_bytes(ctrl_q, false), vec![17]);
        assert_eq!(
            key_bytes(
                crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                false
            ),
            vec![27]
        );
    }

    #[test]
    fn pane_key_routing_forwards_end_and_only_ctrl_end_scrolls() {
        let plain_end = crossterm::event::KeyEvent::new(KeyCode::End, KeyModifiers::NONE);
        let ctrl_end = crossterm::event::KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL);
        let ctrl_v = crossterm::event::KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
        let ctrl_five = crossterm::event::KeyEvent::new(KeyCode::Char('5'), KeyModifiers::CONTROL);
        assert!(matches!(
            route_pane_key(plain_end, false, false),
            PaneKeyRoute::Send(bytes) if bytes == b"\x1b[F"
        ));
        assert!(matches!(
            route_pane_key(ctrl_v, false, false),
            PaneKeyRoute::ArmLiteral
        ));
        assert!(matches!(
            route_pane_key(plain_end, true, false),
            PaneKeyRoute::Send(bytes) if bytes == b"\x1b[F"
        ));
        assert!(matches!(
            route_pane_key(ctrl_end, false, false),
            PaneKeyRoute::ScrollToBottom
        ));
        assert!(matches!(
            route_pane_key(ctrl_end, true, false),
            PaneKeyRoute::Send(bytes) if bytes == b"\x1b[F"
        ));
        assert!(matches!(
            route_pane_key(ctrl_five, true, false),
            PaneKeyRoute::Send(bytes) if bytes == b"\x1d"
        ));
    }

    #[test]
    fn sidebar_hit_testing_ignores_unrendered_rows() {
        assert_eq!(sidebar_hit_target(3, 3, 0, 10, 1), Some(0));
        assert_eq!(sidebar_hit_target(4, 3, 0, 10, 1), None);
        assert_eq!(sidebar_hit_target(2, 3, 0, 10, 1), None);
        assert_eq!(sidebar_hit_target(8, 3, 0, 4, 10), None);
        assert_eq!(sidebar_hit_target(5, 3, 4, 4, 10), Some(6));
    }

    #[test]
    fn baud_prompt_can_use_every_row_without_reserving_a_pane() {
        let area = ratatui::layout::Rect::new(0, 0, 18, 5);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(0)])
            .split(area);
        assert_eq!(rows[0].height, 5);
        assert_eq!(rows[1].height, 0);
        let vt_rows = 1u16;
        let pane_rows = 2u16.min(vt_rows);
        assert_eq!(pane_rows, vt_rows);
    }

    #[test]
    fn gap_header_prioritizes_status_and_client_count_on_small_screens() {
        let device = Device {
            state: "connected".into(),
            baud: "115200".into(),
            control: "available".into(),
            ..Device::default()
        };

        let header = gap_prompt_header(
            &device,
            "3",
            "Daemon unavailable · r reconnects · cached screens retained",
            16,
            2,
        );
        assert_eq!(header, "GAP a=ack MCP3\nr reconnect");
        assert_eq!(prompt_header_height(&header, 16, 5), 2);
        assert_eq!(
            prompt_header_height(&header, 16, 5).min(5_u16.saturating_sub(3).max(1)),
            2
        );
        assert_eq!(
            gap_prompt_header(
                &device,
                "3",
                "Daemon shutting down · cached screens retained",
                32,
                2,
            ),
            "GAP a=ack MCP3 115200\nr reconnect"
        );
        assert_eq!(
            gap_prompt_header(&device, "3", "Input outcome unknown; not replayed", 16, 2),
            "Input? unknown\nNo replay"
        );
        assert_eq!(
            gap_prompt_header(&device, "3", "Input outcome unknown; not replayed", 16, 1),
            "Input? no replay"
        );
        assert_eq!(
            gap_prompt_header_for(
                &device,
                "3",
                "Daemon unavailable · r reconnects",
                16,
                1,
                false
            ),
            "r MCP3"
        );
        assert_eq!(
            gap_prompt_header(
                &device,
                "3",
                "Input outcome unknown; not replayed · Device disconnected",
                16,
                2,
            ),
            "Input? unknown\nNo replay; Enter"
        );
        assert_eq!(
            gap_prompt_header(
                &device,
                "3",
                "Input outcome unknown; not replayed · Daemon unavailable · r reconnects",
                16,
                2,
            ),
            "Input? unknown\nNo replay; r"
        );
        assert_eq!(
            gap_prompt_header(
                &device,
                "3",
                "Paste outcome unknown; remaining bytes were not sent",
                16,
                2,
            ),
            "Partial? MCP3\nRest unsent"
        );
        assert_eq!(
            gap_prompt_header(
                &device,
                "3",
                "Mouse input outcome unknown; not replayed",
                16,
                2,
            ),
            "Mouse? unknown\nNo replay"
        );
        assert_eq!(
            input_warning_header(
                "Input outcome unknown; not replayed · Port list unavailable · cached tabs",
                16,
                2,
                None,
            ),
            "Input? unknown\nNo replay"
        );
        assert_eq!(
            recovery_prompt_header(
                "Input outcome unknown; not replayed · daemon unavailable · r reconnects",
                16,
                2,
            ),
            "Input? unknown\nNo replay; r"
        );
        assert_eq!(
            recovery_prompt_header(
                "Input outcome unknown; not replayed · daemon unavailable · r reconnects",
                16,
                1,
            ),
            "Input? r"
        );
        assert_eq!(
            recovery_prompt_header("daemon unavailable", 16, 1),
            "D lost r"
        );
        assert_eq!(
            recovery_prompt_header("daemon unavailable", 16, 2),
            "Daemon lost\nr reconnect"
        );
        assert_eq!(
            recovery_prompt_header("subscription failed", 16, 2),
            "Subscribe failed\nr reconnect"
        );
        assert_eq!(
            recovery_prompt_header("leave outcome unknown", 16, 2),
            "Leave uncertain\nr reconnect"
        );
        let tiny_warning = input_warning_header(
            "Paste outcome unknown; remaining bytes were not sent · Daemon unavailable · r reconnects",
            8,
            2,
            None,
        );
        assert_eq!(tiny_warning, "Partial?\nRest? r");
        assert_eq!(wrapped_rows(&tiny_warning, 8), 2);
        let paste_warning = "Paste outcome unknown; remaining bytes were not sent";
        assert_eq!(
            input_warning_header(paste_warning, 80, 12, None),
            paste_warning
        );
        assert_eq!(
            input_warning_header(
                "Input outcome unknown; not replayed · Join failed",
                16,
                2,
                None,
            ),
            "Input? unknown\nNo replay"
        );
        assert_eq!(
            gap_prompt_header(
                &device,
                "3",
                "Device disconnected · select and press Enter to reconnect",
                16,
                2,
            ),
            "GAP a=ack MCP3\nEnter reconnect"
        );
        assert_eq!(
            gap_prompt_header(
                &device,
                "3",
                "Output subscription failed; connection closed for cleanup",
                16,
                2,
            ),
            "GAP a=ack MCP3\nSub fail · r"
        );
        assert_eq!(
            recovery_prompt_header(
                "Leave outcome unknown; connection closed for cleanup; r reconnects",
                16,
                2,
            ),
            "Leave uncertain\nr reconnect"
        );
        let combined = "Paste outcome unknown; remaining bytes were not sent · Leave outcome unknown; connection closed for cleanup; r reconnects";
        assert!(status_indicates_input_uncertainty(combined));
        assert_eq!(
            input_warning_header(combined, 16, 2, None),
            "Partial?\nRest unsent L? r"
        );
        let body_cleanup_unknown = "Paste body chunk outcome unknown; remaining bytes were not sent; closing delimiter outcome unknown; application may remain in paste mode · Daemon unavailable · r reconnects";
        assert_eq!(
            input_warning_header(body_cleanup_unknown, 16, 2, None),
            "Partial?\nRest? NR M? r"
        );
        assert_eq!(
            input_warning_header(body_cleanup_unknown, 12, 1, None),
            "P? NR M?r"
        );
        assert_eq!(
            input_warning_header(
                "Paste opening delimiter outcome unknown; closing delimiter outcome unknown; application may remain in paste mode",
                16,
                2,
                None,
            ),
            "Open? unknown\nM? noBody"
        );
        assert_eq!(
            input_warning_header(
                "Paste body sent; closing delimiter outcome unknown; application may remain in paste mode",
                16,
                2,
                None,
            ),
            "Body sent\nPaste mode?"
        );
        assert_eq!(
            input_warning_header(
                "Paste body sent; closing delimiter outcome unknown; application may remain in paste mode",
                16,
                1,
                None,
            ),
            "Body sent; mode?"
        );
        assert_eq!(
            input_warning_header(
                "Paste opening delimiter outcome unknown; closing delimiter outcome unknown; application may remain in paste mode",
                16,
                1,
                None,
            ),
            "Open? M?"
        );
        assert_eq!(
            input_warning_header(
                "Paste body chunk outcome unknown; remaining bytes were not sent; closing delimiter outcome unknown; application may remain in paste mode",
                16,
                1,
                None,
            ),
            "P? NR M?"
        );
        assert_eq!(
            input_warning_header(
                "Input outcome unknown; not replayed · Paste mode outcome unknown; application may remain in paste mode · Daemon unavailable · r reconnects",
                16,
                2,
                None,
            ),
            "Input? unknown\nNo rep; M? r"
        );
        let unresolved_body = "Paste body chunk outcome unknown; remaining bytes were not sent; closing delimiter outcome unknown; application may remain in paste mode";
        let resolved_mode = resolve_paste_mode_warning(unresolved_body);
        assert!(resolved_mode.contains("Paste body chunk outcome unknown"));
        assert!(resolved_mode.contains("remaining bytes were not sent"));
        assert!(resolved_mode.contains("closing delimiter was later sent"));
        assert!(!resolved_mode.contains("application may remain in paste mode"));
        for width in 8..=15 {
            for rows in 1..=2 {
                let headers = [
                    input_warning_header(
                        "Paste body chunk outcome unknown; remaining bytes were not sent · Daemon unavailable · r reconnects",
                        width,
                        rows,
                        None,
                    ),
                    input_warning_header(
                        "Input outcome unknown; not replayed · Device disconnected",
                        width,
                        rows,
                        None,
                    ),
                    input_warning_header(
                        "Paste body chunk outcome unknown; remaining bytes were not sent · Device disconnected",
                        width,
                        rows,
                        None,
                    ),
                    input_warning_header(
                        "Paste body sent; closing delimiter outcome unknown; application may remain in paste mode",
                        width,
                        rows,
                        None,
                    ),
                    input_warning_header(
                        "Paste opening delimiter outcome unknown; closing delimiter outcome unknown; application may remain in paste mode · Daemon unavailable · r reconnects",
                        width,
                        rows,
                        None,
                    ),
                    input_warning_header(
                        "Paste body chunk outcome unknown; remaining bytes were not sent; closing delimiter outcome unknown; application may remain in paste mode · Device disconnected",
                        width,
                        rows,
                        None,
                    ),
                    input_warning_header(
                        "Paste body sent; closing delimiter outcome unknown; application may remain in paste mode · Device disconnected",
                        width,
                        rows,
                        None,
                    ),
                    input_warning_header(
                        "Input outcome unknown; not replayed · Paste mode outcome unknown; application may remain in paste mode · Daemon unavailable · r reconnects",
                        width,
                        rows,
                        None,
                    ),
                    input_warning_header(
                        "Input outcome unknown; not replayed · Paste mode outcome unknown; application may remain in paste mode · Device disconnected",
                        width,
                        rows,
                        None,
                    ),
                    input_warning_header(
                        "Input outcome unknown; not replayed · Paste body chunk outcome unknown; remaining bytes were not sent; closing delimiter outcome unknown; application may remain in paste mode · Daemon unavailable · r reconnects",
                        width,
                        rows,
                        None,
                    ),
                    recovery_prompt_header("Daemon unavailable", width, rows),
                    recovery_prompt_header("Leave outcome unknown", width, rows),
                    recovery_prompt_header("Output subscription failed", width, rows),
                    gap_prompt_header(&device, "3", "Join failed", width, rows),
                    gap_prompt_header(
                        &device,
                        "3",
                        "Device disconnected · select and press Enter to reconnect",
                        width,
                        rows,
                    ),
                ];
                for header in headers {
                    assert!(
                        header.lines().count() <= usize::from(rows),
                        "header exceeded {rows} rows at width {width}: {header:?}"
                    );
                    assert!(
                        header
                            .lines()
                            .all(|line| line.chars().count() <= usize::from(width)),
                        "header exceeded width {width}: {header:?}"
                    );
                }
                for status in [
                    "Input outcome unknown; not replayed · Paste mode outcome unknown; application may remain in paste mode · Daemon unavailable · r reconnects",
                    "Input outcome unknown; not replayed · Paste mode outcome unknown; application may remain in paste mode · Device disconnected",
                    "Input outcome unknown; not replayed · Paste body chunk outcome unknown; remaining bytes were not sent; closing delimiter outcome unknown; application may remain in paste mode · Daemon unavailable · r reconnects",
                ] {
                    let header = input_warning_header(status, width, rows, None);
                    let last_line = header.lines().last().unwrap_or_default();
                    assert!(
                        header.contains("M?"),
                        "paste-mode uncertainty missing at width {width}, rows {rows}: {header:?}"
                    );
                    assert!(
                        ["NR", "NoRep", "No rep", "No replay"]
                            .iter()
                            .any(|marker| header.contains(marker)),
                        "no-replay warning missing at width {width}, rows {rows}: {header:?}"
                    );
                    if status.contains("reconnects") {
                        assert!(last_line.ends_with('r'), "retry action missing: {header:?}");
                    } else {
                        assert!(
                            last_line.ends_with('E') || last_line.ends_with("Enter"),
                            "reconnect action missing: {header:?}"
                        );
                    }
                }
            }
        }
        assert_eq!(
            input_warning_header(
                "Input outcome unknown; not replayed · Device disconnected",
                16,
                1,
                None,
            ),
            "I? no rep; Enter"
        );
    }

    #[test]
    fn baud_prompt_header_reserves_rows_for_wrapped_text() {
        let confirmation =
            " serial-mcp · MCP clients: 1\n Shared baud affects all clients. Confirm y/n";
        let prompt = " serial-mcp · MCP clients: 1\n Baud rate 1–4000000: 4000000 · Enter confirms · Esc cancels";
        let empty_prompt =
            " serial-mcp · MCP clients: 1\n Baud rate 1–4000000:  · Enter confirms · Esc cancels";
        let compact_confirmation = "MCP1\nShared baud affects all\nConfirm y/n";
        let compact_prompt = "MCP1\nBaud:4000000\nRange 1-4000000\nEnter=apply\nEsc=cancel";
        let tiny_confirmation = "Shared baud? y/n";
        let tiny_prompt = "Baud 4000000 Enter apply/Esc cancel";
        let minimum_prompt = "4000000 Enter apply/Esc cancel";
        let minimum_confirmation = "y/n";
        let short_prompt = "Baud:4000000\n1-4000000\nEnter apply/Esc cancel";
        let confirmation_candidates = vec![
            confirmation.to_owned(),
            "MCP1\nShared baud affects all\nConfirm y/n".to_owned(),
            tiny_confirmation.to_owned(),
            minimum_confirmation.to_owned(),
        ];
        let baud_candidates = vec![
            prompt.to_owned(),
            compact_prompt.to_owned(),
            short_prompt.to_owned(),
            tiny_prompt.to_owned(),
            minimum_prompt.to_owned(),
        ];
        assert_eq!(wrapped_rows(confirmation, 50), 2);
        assert_eq!(wrapped_rows(confirmation, 24), 4);
        assert_eq!(wrapped_rows(confirmation, 18), 6);
        assert_eq!(wrapped_rows(prompt, 50), 3);
        assert_eq!(wrapped_rows(prompt, 24), 5);
        assert_eq!(wrapped_rows(prompt, 20), 6);
        assert_eq!(wrapped_rows(prompt, 17), 7);
        assert_eq!(wrapped_rows(empty_prompt, 51), 3);
        assert_eq!(prompt_header_height(compact_confirmation, 18, 8), 4);
        assert_eq!(prompt_header_height(compact_prompt, 18, 8), 5);
        assert_eq!(prompt_header_height(compact_prompt, 18, 7), 5);
        assert_eq!(prompt_header_height(compact_prompt, 18, 6), 5);
        assert_eq!(prompt_header_height(compact_prompt, 18, 5), 5);
        assert_eq!(prompt_header_height(compact_confirmation, 18, 3), 3);
        assert_eq!(prompt_header_height(tiny_confirmation, 18, 3), 1);
        assert_eq!(wrapped_rows(tiny_prompt, 18), 2);
        assert_eq!(wrapped_rows(short_prompt, 18), 4);
        assert_eq!(prompt_header_height(short_prompt, 18, 4), 4);
        assert_eq!(
            fitting_prompt(&baud_candidates, 24, 4),
            Some(short_prompt.to_owned())
        );
        assert_eq!(
            fitting_prompt(&confirmation_candidates, 10, 4),
            Some(tiny_confirmation.to_owned())
        );
        assert_eq!(
            fitting_prompt(&confirmation_candidates, 10, 1),
            Some(minimum_confirmation.to_owned())
        );
        assert_eq!(fitting_prompt(&baud_candidates, 10, 1), None);
        assert_eq!(
            fitting_prompt(&baud_candidates, 16, 5),
            Some(compact_prompt.to_owned())
        );
        assert!(baud_dialog_supported(16, 5));
        assert!(!baud_dialog_supported(10, 1));
        assert_eq!(vt_screen_width(50), 48);
        assert_eq!(vt_screen_width(100), 70);
        let mut digits = String::new();
        for digit in "0000000".chars() {
            assert!(push_baud_digit(&mut digits, digit));
        }
        assert!(!push_baud_digit(&mut digits, '9'));
        assert_eq!(digits, "0000000");
        assert_eq!(prompt_header_height(confirmation, 18, 1), 1);
    }

    #[test]
    fn mouse_reports_follow_the_requested_tracking_mode() {
        use crossterm::event::{MouseButton, MouseEventKind as K};
        let sgr = vt100::MouseProtocolEncoding::Sgr;
        let none = KeyModifiers::NONE;
        assert_eq!(
            encode_mouse(vt100::MouseProtocolMode::Press, sgr, K::Moved, none, 4, 2),
            None
        );
        assert_eq!(
            encode_mouse(
                vt100::MouseProtocolMode::Press,
                sgr,
                K::Down(MouseButton::Left),
                none,
                4,
                2
            )
            .unwrap(),
            b"\x1b[<0;4;2M"
        );
        assert_eq!(
            encode_mouse(
                vt100::MouseProtocolMode::Press,
                sgr,
                K::Up(MouseButton::Left),
                none,
                4,
                2
            ),
            None
        );
        assert_eq!(
            encode_mouse(
                vt100::MouseProtocolMode::ButtonMotion,
                sgr,
                K::Up(MouseButton::Middle),
                none,
                4,
                2
            )
            .unwrap(),
            b"\x1b[<1;4;2m"
        );
        assert_eq!(
            encode_mouse(
                vt100::MouseProtocolMode::ButtonMotion,
                sgr,
                K::Drag(MouseButton::Left),
                none,
                4,
                2
            )
            .unwrap(),
            b"\x1b[<32;4;2M"
        );
        assert_eq!(
            encode_mouse(
                vt100::MouseProtocolMode::AnyMotion,
                sgr,
                K::Moved,
                none,
                4,
                2
            )
            .unwrap(),
            b"\x1b[<35;4;2M"
        );
        let default = vt100::MouseProtocolEncoding::Default;
        assert_eq!(
            encode_mouse(
                vt100::MouseProtocolMode::Press,
                default,
                K::Down(MouseButton::Left),
                none,
                96,
                1,
            )
            .unwrap(),
            [b"\x1b[M".as_slice(), &[32, 128, 33]].concat()
        );
        assert_eq!(
            encode_mouse(
                vt100::MouseProtocolMode::Press,
                default,
                K::Down(MouseButton::Left),
                none,
                223,
                223,
            )
            .unwrap(),
            [b"\x1b[M".as_slice(), &[32, 255, 255]].concat()
        );
        assert_eq!(
            encode_mouse(
                vt100::MouseProtocolMode::PressRelease,
                default,
                K::Up(MouseButton::Middle),
                KeyModifiers::SHIFT,
                4,
                2,
            )
            .unwrap(),
            [b"\x1b[M".as_slice(), &[39, 36, 34]].concat()
        );
        let utf8 = vt100::MouseProtocolEncoding::Utf8;
        assert_eq!(
            encode_mouse(
                vt100::MouseProtocolMode::Press,
                utf8,
                K::Down(MouseButton::Left),
                none,
                2015,
                2015,
            )
            .unwrap(),
            [b"\x1b[M".as_slice(), &[32, 0xdf, 0xbf, 0xdf, 0xbf]].concat()
        );
        assert_eq!(
            encode_mouse(
                vt100::MouseProtocolMode::PressRelease,
                utf8,
                K::Up(MouseButton::Right),
                KeyModifiers::ALT,
                4,
                2,
            )
            .unwrap(),
            [b"\x1b[M".as_slice(), &[43, 36, 34]].concat()
        );
        assert_eq!(
            encode_mouse(
                vt100::MouseProtocolMode::Press,
                utf8,
                K::Down(MouseButton::Left),
                none,
                2016,
                1,
            ),
            None
        );
    }

    struct FakeProcess {
        polls_before_exit: usize,
    }

    impl SpawnedProcess for FakeProcess {
        fn has_exited(&mut self) -> std::io::Result<bool> {
            if self.polls_before_exit == 0 {
                Ok(true)
            } else {
                self.polls_before_exit -= 1;
                Ok(false)
            }
        }
    }

    #[test]
    fn reconnect_retries_start_after_an_early_daemon_exit() {
        use std::cell::Cell;

        let starts = Cell::new(0);
        let elapsed = Cell::new(Duration::ZERO);
        let result = connect_with_daemon_start_policy(
            || {
                if starts.get() >= 2 {
                    Ok(())
                } else {
                    Err("synthetic daemon is draining".into())
                }
            },
            || {
                starts.set(starts.get() + 1);
                Ok(FakeProcess {
                    polls_before_exit: 0,
                })
            },
            || elapsed.get(),
            |duration| elapsed.set(elapsed.get() + duration),
            Duration::from_secs(10),
            5,
        );
        assert_eq!(result, Ok(()));
        assert_eq!(starts.get(), 2);
    }

    #[test]
    fn reconnect_respects_start_attempt_limit_and_deadline() {
        use std::cell::Cell;

        let attempts = Cell::new(0);
        let elapsed = Cell::new(Duration::ZERO);
        let result = connect_with_daemon_start_policy(
            || Err::<(), _>("synthetic daemon unavailable".into()),
            || {
                attempts.set(attempts.get() + 1);
                Ok(FakeProcess {
                    polls_before_exit: 0,
                })
            },
            || elapsed.get(),
            |duration| elapsed.set(elapsed.get() + duration),
            Duration::from_secs(10),
            3,
        );
        assert!(result.is_err());
        assert_eq!(attempts.get(), 3);
        assert!(elapsed.get() < Duration::from_secs(10));

        let attempts = Cell::new(0);
        let elapsed = Cell::new(Duration::ZERO);
        let result = connect_with_daemon_start_policy(
            || Err::<(), _>("synthetic daemon unavailable".into()),
            || {
                attempts.set(attempts.get() + 1);
                Ok(FakeProcess {
                    polls_before_exit: 0,
                })
            },
            || elapsed.get(),
            |duration| elapsed.set(elapsed.get() + duration),
            Duration::from_millis(250),
            100,
        );
        assert!(result.is_err());
        assert!(attempts.get() < 100);
        assert!(elapsed.get() >= Duration::from_millis(250));
    }

    #[test]
    fn hotplug_preserves_returning_tabs_across_status_then_ports_snapshots() {
        let mut devices = (0..MAX_DEVICE_TABS)
            .map(|i| test_device(format!("/dev/test{i}")))
            .collect::<Vec<_>>();
        devices[0].state = "disconnected".into();
        devices[1].state = "disconnected".into();
        devices[2].state = "disconnected".into();
        devices[0].screen.process(b"cached listed output");
        devices[1].screen.process(b"cached status output");
        let mut active = Some(4);
        let mut selected = 5;
        let listed = ["/dev/test0".to_owned()]
            .into_iter()
            .collect::<HashSet<_>>();
        let snapshot = json!({"devices": [
            {"port": "/dev/status-new", "state": "connected", "baudrate": 115200},
            {"port": "/dev/test1", "state": "connected", "baudrate": 9600}
        ]});

        assert!(reconcile_device_tabs(
            &mut devices,
            Some(&snapshot),
            Some(&listed),
            &mut active,
            &mut selected
        ));
        assert_eq!(devices.len(), MAX_DEVICE_TABS);
        assert_eq!(devices[0].port, "/dev/test0");
        assert_eq!(devices[0].state, "available");
        assert_eq!(
            devices[0].screen.screen().cell(0, 0).unwrap().contents(),
            "c"
        );
        assert_eq!(devices[1].port, "/dev/test1");
        assert_eq!(devices[1].state, "connected");
        assert_eq!(
            devices[1].screen.screen().cell(0, 0).unwrap().contents(),
            "c"
        );
        assert!(devices
            .iter()
            .any(|device| device.port == "/dev/status-new"));
        assert!(!devices.iter().any(|device| device.port == "/dev/test2"));
        assert_eq!(active, Some(3));
        assert_eq!(selected, 4);
    }

    #[test]
    fn incomplete_port_snapshot_preserves_every_cached_tab() {
        let mut devices = (0..MAX_DEVICE_TABS)
            .map(|i| test_device(format!("/dev/test{i}")))
            .collect::<Vec<_>>();
        devices[0].state = "disconnected".into();
        devices[0].screen.process(b"cached output");
        let mut active = Some(2);
        let mut selected = 3;
        let snapshot = json!({"devices": [
            {"port": "/dev/status-new", "state": "connected", "baudrate": 115200},
            {"port": "/dev/test1", "state": "connected", "baudrate": 115200, "control": "granted"}
        ]});

        assert!(!reconcile_device_tabs(
            &mut devices,
            Some(&snapshot),
            None,
            &mut active,
            &mut selected
        ));
        assert_eq!(devices.len(), MAX_DEVICE_TABS);
        assert_eq!(devices[0].port, "/dev/test0");
        assert_eq!(devices[0].state, "disconnected");
        assert_eq!(
            devices[0].screen.screen().cell(0, 0).unwrap().contents(),
            "c"
        );
        assert_eq!(devices[1].state, "connected");
        assert_eq!(devices[1].baud, "115200");
        assert_eq!(devices[1].control, "granted");
        assert!(!devices
            .iter()
            .any(|device| device.port == "/dev/status-new"));
        assert_eq!(active, Some(2));
        assert_eq!(selected, 3);

        let malformed_snapshot = json!({"not_devices": []});
        let listed = ["/dev/available".to_owned()]
            .into_iter()
            .collect::<HashSet<_>>();
        assert!(reconcile_device_tabs(
            &mut devices,
            Some(&malformed_snapshot),
            Some(&listed),
            &mut active,
            &mut selected
        ));
        assert_eq!(devices.len(), MAX_DEVICE_TABS);
        assert_eq!(devices[0].port, "/dev/test0");
        assert_eq!(
            devices[0].screen.screen().cell(0, 0).unwrap().contents(),
            "c"
        );
        assert_eq!(active, Some(2));
        assert_eq!(selected, 3);
    }

    #[test]
    fn incomplete_port_snapshot_adds_status_devices_without_eviction() {
        let mut devices = vec![
            test_device("/dev/test0".into()),
            test_device("/dev/test1".into()),
        ];
        devices[0].screen.process(b"cached screen");
        let mut active = Some(0);
        let mut selected = 1;
        let snapshot = json!({"devices": [
            {"port": "/dev/status-new", "state": "connected", "baudrate": 115200},
            {"port": "/dev/test1", "state": "connected", "baudrate": 9600}
        ]});

        assert!(reconcile_device_tabs(
            &mut devices,
            Some(&snapshot),
            None,
            &mut active,
            &mut selected
        ));
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].port, "/dev/test0");
        assert_eq!(
            devices[0].screen.screen().cell(0, 0).unwrap().contents(),
            "c"
        );
        assert_eq!(devices[2].port, "/dev/status-new");
        assert_eq!(devices[2].state, "connected");
        assert_eq!(devices[1].state, "connected");
        assert_eq!(active, Some(0));
        assert_eq!(selected, 1);
    }

    #[test]
    fn hotplug_evicts_only_inactive_disconnected_tabs() {
        let mut devices = (0..MAX_DEVICE_TABS)
            .map(|i| test_device(format!("/dev/test{i}")))
            .collect::<Vec<_>>();
        devices[0].state = "disconnected".into();
        devices[1].state = "disconnected".into();
        let mut active = Some(1);
        let mut selected = 2;
        let protected = HashSet::new();
        assert!(add_device(
            &mut devices,
            "/dev/new",
            &mut active,
            &mut selected,
            &protected
        ));
        assert_eq!(devices.len(), MAX_DEVICE_TABS);
        assert_eq!(devices[0].port, "/dev/test1");
        assert_eq!(devices[MAX_DEVICE_TABS - 1].port, "/dev/new");
        assert_eq!(active, Some(0));
        assert_eq!(selected, 1);
        assert!(!add_device(
            &mut devices,
            "/dev/another",
            &mut active,
            &mut selected,
            &protected
        ));
    }

    #[test]
    fn base64_input_round_trips_without_local_echo() {
        let input = b"synthetic input\r";
        assert_eq!(base64_decode(&base64_encode(input)).unwrap(), input);
    }
}
