use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers, MouseEventKind},
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
                cols.saturating_sub(30).max(1),
                SCROLLBACK,
            ),
            gap: false,
            new_output: false,
            selection: None,
            selecting: false,
            joined: false,
        }
    }
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
                println!("serial-mcp-terminal — shared serial multi-device TUI\\nUsage: serial-mcp-terminal [--help|--version]\\nSidebar: Up/Down or j/k, Enter, q/Ctrl-Q. Pane: Esc/Ctrl-] leaves; Ctrl-V then Ctrl-]/Esc sends literally; Ctrl-B opens shared baud controls; Ctrl-End scrolls to bottom.");
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
    let mut selected = 0usize;
    let mut count = hello["mcp_clients"].as_u64().unwrap_or(0).to_string();
    let mut status = "Ready".to_string();
    let mut baud_prompt: Option<String> = None;
    let mut baud_change_pending = false;
    let mut baud_confirm = false;
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
        crossterm::event::EnableMouseCapture
    ) {
        let _ = execute!(
            out,
            crossterm::event::DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
        return Err(e.to_string());
    }
    let result = (|| -> Result<(), String> {
        let mut term = Terminal::new(CrosstermBackend::new(out)).map_err(|e| e.to_string())?;
        loop {
            if STOP.load(Ordering::SeqCst) {
                break;
            }
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
                if count == "?" {
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
                if let Some(i) = active.take() {
                    devs[i].joined = false;
                    devs[i].gap = true;
                }
                status = "Daemon unavailable · r reconnects · cached screens retained".into();
            }
            if snapshot.is_some() && !snapshot_ok {
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
            if !tabs_ok {
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
                }
            }
            if c.overflow {
                c.overflow = false;
                if let Some(i) = active {
                    devs[i].gap = true;
                }
                status = "Daemon event queue overflow — output/settings may be incomplete".into();
            }
            while let Some(ev) = c.events.pop_front() {
                match ev["event"].as_str().unwrap_or("") {
                    "shutdown" => {
                        daemon_lost = true;
                        if let Some(i) = active.take() {
                            devs[i].joined = false;
                            devs[i].gap = true;
                        }
                        status = "Daemon shutting down · cached screens retained".into();
                    }
                    "event_gap" => {
                        if let Some(i) = active {
                            devs[i].gap = true;
                        }
                        status = "Daemon event gap · state may be incomplete".into();
                    }
                    "settings_gap" => {
                        if let Some(i) = active {
                            devs[i].gap = true;
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
                            d.gap = true;
                        }
                        "device" if ev["state"] == "disconnected" => {
                            d.state = "disconnected".into();
                            d.joined = false;
                            d.gap = true;
                            if Some(idx) == active {
                                let _ = c.req(json!({"op":"leave","port":d.port}));
                                active = None;
                                selected = idx;
                            }
                            status =
                                "Device disconnected · select and press Enter to reconnect".into();
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
                    let _ = c.req(json!({"op":"subscribe","port":devs[i].port}));
                    devs[i].joined = true;
                }
            }
            let (term_width, term_height) = crossterm::terminal::size().unwrap_or((80, 24));
            let narrow = term_width < 64;
            let sidebar_width = if narrow {
                if active.is_some() {
                    0
                } else {
                    term_width.min(26)
                }
            } else {
                28
            };
            let prompt_header = if baud_confirm {
                let candidates = [
                    format!(" serial-mcp · MCP clients: {count}\n Shared baud affects all clients. Confirm y/n"),
                    format!("MCP{count}\nShared baud affects all\nConfirm y/n"),
                    "Shared baud? y/n".to_owned(),
                    "y/n".to_owned(),
                ];
                Some(fitting_prompt(&candidates, term_width, term_height))
            } else {
                baud_prompt.as_ref().map(|baud| {
                    let candidates = [
                        format!(" serial-mcp · MCP clients: {count}\n Baud rate 1–4000000: {baud} · Enter confirms · Esc cancels"),
                        format!("MCP{count}\nBaud:{baud}\nRange 1-4000000\nEnter=apply\nEsc=cancel"),
                        format!("Baud:{baud}\n1-4000000\nEnter apply/Esc cancel"),
                        format!("Baud {baud} Enter apply/Esc cancel"),
                        format!("E/Esc {baud}"),
                    ];
                    fitting_prompt(&candidates, term_width, term_height)
                })
            };
            let header_height = prompt_header
                .as_ref()
                .map(|header| prompt_header_height(header, term_width, term_height))
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
            for device in &mut devs {
                device.screen.screen_mut().set_size(
                    screen_rows,
                    term_width
                        .saturating_sub(sidebar_width)
                        .saturating_sub(2)
                        .max(1),
                );
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
                        " serial-mcp · MCP clients: {count}    {}    {}",
                        status, device_status
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
                            "{} {}  {} · {} baud · control {}",
                            if Some(i) == active {
                                "●"
                            } else if i == selected {
                                "›"
                            } else {
                                " "
                            },
                            d.port,
                            d.state,
                            d.baud,
                            d.control
                        ))
                    })
                    .collect();
                f.render_widget(
                    List::new(items).block(
                        Block::default()
                            .title(" Devices · Enter join · d Disconnect · q quit ")
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
                                    " Terminal · Esc Disconnect · Ctrl-B Change baud · Ctrl-End Bottom "
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
                        for device in &mut devs {
                            device.screen.screen_mut().set_size(
                                rows.saturating_sub(4).max(1),
                                cols.saturating_sub(30).max(1),
                            );
                        }
                    }
                    Event::Key(k) if k.code == KeyCode::Char('q') && active.is_none() => break,
                    Event::Key(k) if k.code==KeyCode::Char('r') && daemon_lost => {
                        match connect_or_start_daemon(){Ok(mut new_client)=>match new_client.req(json!({"op":"hello","role":"human","name":"rust-human-terminal","pid":std::process::id()})){Ok(h) if h["mcp_clients"].as_u64().is_some()=>{if let Some(i)=active.take(){devs[i].joined=false;devs[i].gap=true;}count=h["mcp_clients"].as_u64().unwrap_or(0).to_string();c=new_client;daemon_lost=false;status="Reconnected · select device explicitly".into();},_=>status="Daemon reconnect failed".into()},Err(_)=>status="Daemon reconnect failed".into()}
                    }
                    Event::Key(k)
                        if ((k.code == KeyCode::Char(']')
                            && k.modifiers.contains(KeyModifiers::CONTROL))
                            || (k.code == KeyCode::Char('5')
                                && k.modifiers.contains(KeyModifiers::CONTROL))
                            || k.code == KeyCode::Char('\x1d'))
                            && !literal_next =>
                    {
                        baud_prompt=None;baud_change_pending=false;baud_confirm=false;
                        if let Some(i) = active.take() {
                            let _ = c.req(json!({"op":"unsubscribe","port":devs[i].port}));
                            let _ = c.req(json!({"op":"leave","port":devs[i].port}));
                            devs[i].joined = false;
                            devs[i].gap = true;
                        }
                        status = "Sidebar".into()
                    }
                    Event::Key(k) if baud_confirm => {
                        baud_confirm=false;
                        if k.code==KeyCode::Char('y') { baud_change_pending=true; baud_prompt=Some(String::new()); status="Enter shared baud rate".into(); }
                        else { status="Baud change cancelled".into(); }
                    }
                    Event::Key(k) if baud_prompt.is_some() => {
                        match k.code {
                            KeyCode::Char(c) if c.is_ascii_digit() => { baud_prompt.as_mut().unwrap().push(c) }
                            KeyCode::Char(_) => { baud_prompt=None; baud_change_pending=false; status="Invalid baud rate; digits only".into(); }
                            KeyCode::Backspace => {
                                baud_prompt.as_mut().unwrap().pop();
                            }
                            KeyCode::Esc => {
                                baud_prompt = None;
                                baud_change_pending=false;
                                status = "Baud entry cancelled".into();
                            }
                            KeyCode::Enter => {
                                let changing=std::mem::take(&mut baud_change_pending);
                                let raw = baud_prompt.take().unwrap();
                                match raw.parse::<u32>() {
                                    Ok(baud) if (1..=4_000_000).contains(&baud) => {
                                        let i = active.unwrap_or(selected);
                                        if changing {
                                            match c.req(json!({"op":"human_set_baudrate","port":devs[i].port,"baudrate":baud})) {
                                                Ok(result)=>{devs[i].baud=result["baudrate"].as_u64().map(|v|v.to_string()).unwrap_or(baud.to_string());status="Shared baud changed; other participants notified".into();},
                                                Err(e)=>status=format!("Baud change failed: {e}")
                                            }
                                        } else {
                                            match c.req(json!({"op":"join","port":devs[i].port,"baudrate":baud})) {
                                                Ok(joined) => { devs[i].device_id=joined["device_id"].as_str().map(str::to_owned); devs[i].baud=joined["baudrate"].as_u64().map(|v|v.to_string()).unwrap_or(devs[i].baud.clone()); let _=c.req(json!({"op":"subscribe","port":devs[i].port})); devs[i].joined=true; devs[i].state="connected".into(); active=Some(i); status="Connected · Ctrl-] leaves".into(); },
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
                    Event::Key(k) if active.is_none() => match k.code {
                        KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
                        KeyCode::Down | KeyCode::Char('j') => { selected = (selected + 1).min(devs.len().saturating_sub(1)) }
                        KeyCode::Char('d') if !devs.is_empty() => {
                            let i=selected; let _=c.req(json!({"op":"leave","port":devs[i].port}));
                            devs[i].joined=false;devs[i].gap=true;status="Disconnected from selected device".into();
                        }
                        KeyCode::Enter if !devs.is_empty() => {
                            let i = selected;
                            if devs[i].state == "connected" {
                                match c.req(json!({"op":"join","port":devs[i].port})) {
                                    Ok(joined) => {
                                        devs[i].device_id=joined["device_id"].as_str().map(str::to_owned);
                                        let _ = c.req(json!({"op":"subscribe","port":devs[i].port}));
                                        devs[i].joined = true;
                                        active = Some(i);
                                        status = "Connected · Ctrl-] leaves".into();
                                    }
                                    Err(e) => status = format!("Join failed: {e}"),
                                }
                            } else {
                                baud_prompt = Some(String::new());
                                status = "Enter baud rate".into();
                            }
                        }
                        _ => {}
                    },
                    Event::Key(k)
                        if active.is_some()
                            && matches!(
                                route_pane_key(k, literal_next, false),
                                PaneKeyRoute::ScrollToBottom
                            ) => {
                        if let Some(i) = active {
                            devs[i].screen.screen_mut().set_scrollback(0);
                            devs[i].new_output = false;
                        }
                    }
                    Event::Key(k) if active.is_some() && k.code == KeyCode::Esc && !literal_next => {
                        if let Some(i) = active.take() {
                            let _ = c.req(json!({"op":"unsubscribe","port":devs[i].port}));
                            let _ = c.req(json!({"op":"leave","port":devs[i].port}));
                            devs[i].joined = false;
                            devs[i].gap = true;
                        }
                        status = "Sidebar".into();
                    }
                    Event::Key(k) if active.is_some() && !literal_next && (k.code==KeyCode::Char('b') || k.code==KeyCode::Char('B')) && k.modifiers.contains(KeyModifiers::CONTROL) => {
                        baud_confirm=true;
                        status="Changing baud affects all participants and may disrupt active AI work. Confirm y/n".into();
                    }
                    Event::Key(k) => {
                        if let Some(i) = active {
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
                                    status = format!("Input outcome unknown: {e}; not replayed");
                                }
                            }
                        }
                    }
                    Event::Paste(text) if active.is_some() => {
                        if let Some(i) = active {
                            for chunk in text.as_bytes().chunks(64 * 1024) {
                                if let Err(e)=c.req(json!({"op":"input","port":devs[i].port,"data":base64_encode(chunk)})) {
                                    status=format!("Paste outcome unknown: {e}; remaining bytes were not sent"); break;
                                }
                            }
                        }
                    }
                    Event::Mouse(m) if active.is_some() && pane_height > 0 && m.column>=sidebar_width && m.row>=pane_content_top && m.row<term_height.saturating_sub(1) && !m.modifiers.contains(KeyModifiers::SHIFT) && active.is_some_and(|i|devs[i].screen.screen().mouse_protocol_mode()!=vt100::MouseProtocolMode::None) => {
                        if let Some(i)=active {
                            let screen=devs[i].screen.screen();
                            let col=m.column.saturating_sub(sidebar_width).max(1);
                            let row = m
                                .row
                                .saturating_sub(header_height)
                                .saturating_add(screen_row_offset)
                                .max(1);
                            if let Some(bytes)=encode_mouse(screen.mouse_protocol_mode(),screen.mouse_protocol_encoding(),m.kind,m.modifiers,col,row) {
                                if let Err(e)=c.req(json!({"op":"input","port":devs[i].port,"data":base64_encode(&bytes)})){status=format!("Mouse input outcome unknown: {e}; not replayed");}
                            }
                        }
                    }
                    Event::Mouse(m) if active.is_some() && pane_height > 0 && (m.modifiers.contains(KeyModifiers::SHIFT) || active.is_some_and(|i|devs[i].screen.screen().mouse_protocol_mode()==vt100::MouseProtocolMode::None)) && m.kind==MouseEventKind::Down(crossterm::event::MouseButton::Left) && m.column>=sidebar_width && m.row>=pane_content_top && m.row<term_height.saturating_sub(1) => {
                        if let Some(i)=active { let pos=(m.row.saturating_sub(pane_content_top).saturating_add(screen_row_offset),m.column.saturating_sub(sidebar_width+1)); devs[i].selection=Some((pos,pos)); devs[i].selecting=true; }
                    }
                    Event::Mouse(m) if active.is_some() && pane_height > 0 && (m.modifiers.contains(KeyModifiers::SHIFT) || active.is_some_and(|i|devs[i].screen.screen().mouse_protocol_mode()==vt100::MouseProtocolMode::None)) && m.kind==MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                        if let Some(i)=active { if devs[i].selecting { if let Some((start,_))=devs[i].selection {let pos=(m.row.saturating_sub(pane_content_top).saturating_add(screen_row_offset),m.column.saturating_sub(sidebar_width+1));devs[i].selection=Some((start,pos));} } }
                    }
                    Event::Mouse(m) if active.is_some() && m.kind==MouseEventKind::Up(crossterm::event::MouseButton::Left) => {if let Some(i)=active{devs[i].selecting=false;}}
                    Event::Mouse(m) if active.is_some() && pane_height > 0 && m.column>=sidebar_width && m.kind == MouseEventKind::ScrollUp => {
                        if let Some(i) = active {
                            let s = devs[i].screen.screen_mut();
                            let n = s.scrollback();
                            s.set_scrollback(n.saturating_add(3));
                            devs[i].new_output = true;
                        }
                    }
                    Event::Mouse(m) if active.is_some() && pane_height > 0 && m.column>=sidebar_width && m.kind == MouseEventKind::ScrollDown => {
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
                        if pane_height > 0 && m.column < sidebar_width && m.row>=pane_content_top && m.row<term_height.saturating_sub(1) {
                            if let Some(i) = active.take() {
                                let _ = c.req(json!({"op":"unsubscribe","port":devs[i].port}));
                                let _ = c.req(json!({"op":"leave","port":devs[i].port}));
                                devs[i].joined = false;
                                devs[i].gap = true;
                            }
                            selected = sidebar_offset.saturating_add((m.row as usize).saturating_sub(pane_content_top as usize)).min(devs.len().saturating_sub(1));
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
        let _ = c.req(json!({"op":"leave","port":devs[i].port}));
    }
    let mut out = std::io::stdout();
    let _ = execute!(
        out,
        crossterm::event::DisableMouseCapture,
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

fn fitting_prompt(candidates: &[String], width: u16, height: u16) -> String {
    if let Some(candidate) = candidates
        .iter()
        .find(|candidate| wrapped_rows(candidate, width) <= height)
    {
        return candidate.clone();
    }
    candidates
        .iter()
        .min_by_key(|candidate| candidate.chars().count())
        .map(|candidate| candidate.chars().take(width.max(1) as usize).collect())
        .unwrap_or_default()
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

    fn test_device(port: String) -> Device {
        let mut device = Device::new(port);
        device.screen.screen_mut().set_size(20, 80);
        device
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
        let minimum_prompt = "E/Esc 4000000";
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
        assert_eq!(fitting_prompt(&baud_candidates, 24, 4), short_prompt);
        assert_eq!(
            fitting_prompt(&confirmation_candidates, 10, 4),
            tiny_confirmation
        );
        assert!(wrapped_rows(&fitting_prompt(&baud_candidates, 10, 4), 10) <= 4);
        assert_eq!(
            fitting_prompt(&confirmation_candidates, 10, 1),
            minimum_confirmation
        );
        assert!(wrapped_rows(&fitting_prompt(&confirmation_candidates, 10, 1), 10) <= 1);
        assert!(!fitting_prompt(&baud_candidates, 10, 1).is_empty());
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
