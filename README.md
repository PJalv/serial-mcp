# Generic Serial MCP

A lightweight native Rust/rmcp stdio frontend for a shared Python serial daemon. It does not assume a specific device, shell, login flow, prompt, or command language. The daemon remains Python and the human terminal is a native Rust TUI; normal MCP launches run the built executable without starting Python/FastMCP frontend code. Wheel installs use a tiny Python console-script trampoline that immediately `exec`s the native executable.

The shared application uses a per-user Python daemon and a native Rust MCP
frontend; the human terminal is a separate native Rust TUI. AI tools pass an explicit `port` to every device-scoped operation;
several devices can be controlled independently. `connect(port, baudrate)` opens
or joins a device and requests control. A busy connection joins read-only, and
modifying operations require an explicit control-granting `connect`. The shared
app does not expose raw tio sockets or companion ptys. The human terminal uses
the daemon's human-client surface; see [docs/human-terminal.md](docs/human-terminal.md).

Repo: https://github.com/alvinla264/serial-mcp

## Quick start

Choose a source-based launcher or build a native executable from a local checkout:

| Source | Command |
|---|---|
| **GitHub (recommended)** | `uvx --from git+https://github.com/alvinla264/serial-mcp serial-mcp` |
| **Local source build** | `cargo build --release --locked --manifest-path rust-client/Cargo.toml --target-dir build/rust-target`, then run `build/rust-target/release/serial-mcp-rust-client` |
| **Local checkout + nix** | `nix run github:alvinla264/serial-mcp` — or `nix run /path/to/serial-mcp` |

Notes:

- Source-based `uvx` and pip installations compile the Rust MCP frontend and terminal once while building the wheel; Cargo and a Rust toolchain must be available during installation. The installed Python console-script trampoline immediately replaces itself with the bundled native executable; it never invokes Cargo or Python's MCP framework during normal launches. Nix installs the binary directly.
- Linux and WSL are the supported Rust frontend targets. FastMCP remains a dependency for the explicit `serial-mcp-legacy` in-process server; it is not the shared-daemon stdio frontend.
- The Rust MCP launcher and the Rust human terminal both speak daemon IPC v2. The Python parity oracle remains a legacy-only v1 test adapter.
- `serial-mcp` is the Rust/rmcp stdio frontend of the shared daemon (see
  [Shared daemon](#shared-daemon-protocol-v2)). The previous in-process server is
  `serial-mcp-legacy` (`server.py`). Status and setup: [docs/mcp-frontend.md](docs/mcp-frontend.md).
- Rust frontend integration uses MCP stdio with isolated v2 daemons and fake
  PTYs. Build and verification details are in
  [frontend status](docs/mcp-frontend.md).
- Serial permissions still apply outside nix: your user needs `dialout` group
  membership (or udev rules) to open `/dev/ttyUSB0`.

### Register with your AI agent

All agents take a command + args; pick the launcher form from the table above.
The examples below use the GitHub source so there's no local path dependency —
swap in a local-path form if you prefer a checkout. `alvinla264/serial-mcp`
can be forked/replaced with your own.

Pin a specific version by appending `@<tag-or-commit>` to the git URL, e.g.
`git+https://github.com/alvinla264/serial-mcp@v0.2.0`, otherwise the branch
tip is re-resolved on each start.

**pi** (`~/.pi/agent/mcp.json`) — GitHub via uvx:

```json
{
  "mcpServers": {
    "serial-mcp": {
      "transport": "stdio",
      "command": "uvx",
      "args": [
        "--from",
        "git+https://github.com/alvinla264/serial-mcp",
        "serial-mcp"
      ]
    }
  }
}
```

pi — local checkout via nix: `"command": "nix"`,
`"args": ["run", "/path/to/serial-mcp"]`.

**Claude Code** (`~/.claude.json` or project `.mcp.json`):

```bash
claude mcp add serial-mcp -- uvx --from git+https://github.com/alvinla264/serial-mcp serial-mcp
```

**Codex CLI** (`~/.codex/config.toml`):

```toml
[mcp_servers.serial-mcp]
command = "uvx"
args = ["--from", "git+https://github.com/alvinla264/serial-mcp", "serial-mcp"]
```

**OpenCode** (`~/.config/opencode/opencode.json`):

```json
{
  "mcp": {
    "serial-mcp": {
      "type": "local",
      "command": [
        "uvx",
        "--from",
        "git+https://github.com/alvinla264/serial-mcp",
        "serial-mcp"
      ]
    }
  }
}
```

**Gemini CLI** (`~/.gemini/settings.json`):

```json
{
  "mcpServers": {
    "serial-mcp": {
      "command": "uvx",
      "args": [
        "--from",
        "git+https://github.com/alvinla264/serial-mcp",
        "serial-mcp"
      ]
    }
  }
}
```

**Cursor** (`~/.cursor/mcp.json`) — same JSON shape as Gemini's: `"command":
"uvx"`, `"args": ["--from", "git+https://github.com/alvinla264/serial-mcp",
"serial-mcp"]`.

**VS Code** (Copilot Chat MCP support, `.vscode/mcp.json` in the workspace or
`~/.vscode/mcp.json` for all projects) — note the `"servers"` wrapper and
`"type": "stdio"`:

```json
{
  "servers": {
    "serial-mcp": {
      "type": "stdio",
      "command": "uvx",
      "args": [
        "--from",
        "git+https://github.com/alvinla264/serial-mcp",
        "serial-mcp"
      ]
    }
  }
}
```

Reload with the `MCP: List Servers` command (or restart VS Code), then start
Copilot Chat in agent mode and ask it to connect to the serial device.

Then reload/restart the agent so it picks up the server and ask it to connect
to the serial device by its explicit port path. The shared app does not join
tio sessions; humans use `serial-mcp-terminal` for direct console access.

### Legacy standalone sessions (not shared-daemon mode)

The following raw-byte tio socket workflows and tool reference apply only to the
legacy in-process adapter. They are not supported by the shared Rust application,
which uses protocol v2 daemon IPC and has no `attach`, `attach_socket`,
`session_socket`, or companion-pty route.

#### You own it, the agent attaches (recommended)

Start tio yourself. It holds the device and serves a socket; the agent joins as
a client whenever you ask it to.

```bash
tio -S unix:/tmp/serial-mcp-ttyUSB0.sock /dev/ttyUSB0
```

Then tell the agent to attach — it calls `attach()` (auto-detects the socket) or
`connect(attach_socket="/tmp/serial-mcp-ttyUSB0.sock")`.

Why this is usually better:

- **The agent cannot lock you out.** It holds no port; `disconnect` only detaches
  it, and your tio session keeps running.
- **Several agent sessions can share one device.** They all attach to your tio, so
  nothing races for the port or for the session paths. Everyone sees all device
  output, whoever caused it.
- **You see everything first.** Including the boot messages from before the agent
  was involved.

> Sharing is for **coexistence, not for simultaneous typing.** tio forwards each
> client's bytes to the port as they arrive and does not buffer per client, so if
> two clients write at the same instant their commands interleave — `echo a` from
> one and `echo b` from the other can reach the device as `echoecho a` and fail.
> Have one agent (or you) drive the device at a time; the others can watch. There
> is no locking in tio's socket protocol, so this cannot be enforced from here.

> tio 3.9+ **requires the `unix:` prefix**: `-S /tmp/x.sock` fails with "Invalid
> socket scheme". Use `-S unix:/tmp/x.sock`.
>
> Run tio in a real terminal. With stdin redirected (a pipe or `/dev/null`) it
> prints `Disconnected` and exits immediately, and nothing is relayed.

#### The agent owns it, you attach

The agent calls `connect(port="/dev/ttyUSB0")`, which opens the device and serves
a session socket plus a **companion pty**. You then attach with any tty tool:

    tio /tmp/serial-mcp-ttyUSB0        # or screen, minicom, cat/echo, ...

The pty (symlinked next to the socket, same name without `.sock`) exists so real
terminal tools can attach as if it were the device: device output streams in, your
typing goes to the device. With tio you also get timestamps and the ctrl-t command
menu (ctrl-t q quits). You can re-attach anytime without reconnecting the agent.

Or poke the session directly (same raw protocol):

    echo "ls" | nc -UN /tmp/serial-mcp-ttyUSB0.sock

`disconnect` closes the device and drops all attached clients.

> **Direct connections refuse an already-open serial device.** On Linux the
> server claims `TIOCEXCL` before configuring/reading the tty and checks visible
> open handles by device number, so symlink aliases are covered. A process that
> already owns the tty (including `tio`) is reported with a PID/name and is not
> read, flushed, disconnected, or reconfigured. If it serves a tio socket, use
> `attach`; otherwise ask its owner to close the port before retrying. An
> additional device-number advisory lock coordinates serial-mcp instances only;
> it is not treated as proof of ownership. Direct open fails closed on platforms
> where this Linux exclusivity/owner check is unavailable. Privileged processes
> able to bypass `TIOCEXCL` remain an OS-level limitation.
>
> **A live session is never taken over.** If a session socket is already being
> served — by your own tio, or by another agent — `connect(port=...)` refuses
> instead of unlinking it and binding its own, and tells you to join it with
> `attach` (or to pass a different `session_socket` path). This matters because
> taking the path over would leave the previous owner running, still holding the
> device, but unreachable — silently breaking the session you were using.
>
> If an agent reports *"session socket ... is already served"* or *"... is
> already held"*, it does not need a fix: it should `attach` to the existing
> session, or you should quit the holder first.
>
> In this direction, run **one** agent session per serial device. Attach mode is
> exempt: there your tio owns the port and the session paths, and any number of
> agents can join it.

## Legacy in-process tool reference

This reference documents `serial-mcp-legacy` (`server.py`), not the shared Rust
MCP frontend. Its single-device and tio-attach behavior is retained only for
legacy users.

- `connect(port=None, baudrate=115200, timeout=0.1, prompt_regex=None,
  session_socket="auto", attach_socket=None)`: opens the serial port (owning it),
  or joins a session someone else serves. Pass exactly one of `port` or
  `attach_socket`:

  - `port=...` — open and own the device, then serve the session socket.
    `session_socket` defaults to "auto", deriving
    `/tmp/serial-mcp-<devname>.sock`; pass an explicit path or `""` to disable.
    Calling it again for the same device reuses the existing session (and keeps
    anyone attached to it).
  - `attach_socket=...` — join a session already being served (typically your
    own `tio -S unix:/tmp/serial-mcp-<dev>.sock <device>`). No port is opened and
    no session socket or pty is created; `disconnect` detaches without
    disturbing the owner.

  `prompt_regex` is an optional default terminator regex for command completion.
- `attach(attach_socket=None, prompt_regex=None)`: convenience wrapper for
  joining a session. Without a path it auto-detects live session sockets and
  requires exactly one, so an ambiguous setup is reported rather than guessed at.
  Pass the path explicitly when more than one session is live.
- `disconnect()`: closes the port, stops the reader, and drops session clients.
  In attach mode it only detaches: the owner's session keeps running.
- `send_command(text, line_ending="lf", timeout=5.0, idle_timeout=0.2,
  terminator_regex=None, encoding="utf-8", max_lines=0)`: sends `text` plus an
  explicit line ending (`none`, `lf`, `cr`, or `crlf`) and returns bytes observed
  after the send point. Completion uses `terminator_regex` (or the connection
  default) if provided; otherwise it returns after `idle_timeout` seconds of
  silence, bounded by the overall `timeout`. `max_lines` caps how much comes back
  (see below).
- `read_output(timeout=0.0, idle_timeout=0.2, encoding="utf-8", max_lines=0)`:
  consumes and returns buffered unsolicited/asynchronous output. With a positive
  timeout it waits for arriving data and then for idle silence, never beyond
  timeout. `max_lines` caps how much comes back (see below).
- `clear_output()`: discards currently buffered output.
- `set_prompt_pattern(regex_pattern=None)`: sets or clears the default regex
  terminator used by `send_command`.
- `view_io(lines=50, direction="all", hex_mode=False, encoding="utf-8")`: tio-style
  timestamped transcript of recent I/O. Shows `[TX:ai]` (what the AI sent),
  `[TX:user]` (what an attached user typed), and `[RX]` (device output).
  `direction` filters to `tx`, `rx`, or `all`; `hex_mode=True` renders raw bytes
  in hex (like `tio --hex`).
- `list_serial_ports()`: lists available serial ports with descriptions
  (like `tio --list`).
- `tio_info()`: connection status. In direct mode: the session socket path,
  attached client count, and attach instructions. In attach mode: which session is
  joined, that this server holds no port, and that detaching leaves it running.

### Limiting returned output (`max_lines`)

Console output is mostly noise — boot banners, repeated log spew — and the part
worth reading is at the *end*: the prompt, the command result, the error. Both
`send_command` and `read_output` accept `max_lines` so the AI can ask for just the
tail instead of dragging thousands of irrelevant lines into context:

- `max_lines=0` (the default) returns everything, unchanged.
- `max_lines=N` keeps the **last** N lines and prepends a marker such as
  `[... 12 earlier lines omitted ...]`, so a truncated result is never mistaken
  for the whole reply.
- Lines are counted on the decoded text; a partial final line without a trailing
  newline still counts as one line and is never dropped.

Use it when you expect verbose output and only care about the outcome, e.g.
`send_command("dmesg", max_lines=30)` or draining an accumulated boot log with
`read_output(max_lines=20)`. For byte-exact or hex views, use `view_io` instead.

### Bootloader entry (U-Boot)

Automated login -> reboot -> autoboot-interrupt flow:

- `enter_bootloader(...)`: logs in if the device sits at a login prompt,
  reboots, then sends interrupt keys at a paced interval during the autoboot
  window until the U-Boot prompt appears. Pacing (`interval`, default 50ms)
  matters: flooding a U-Boot console can overflow the RX buffer and *drop*
  interrupt characters. Hard caps everywhere: `max_attempts` per pass and
  `overall_timeout` for the whole operation; if an `abort_regex` (e.g.
  `Login:`) shows the device booted fully, it logs in and reboots again
  instead of tapping into a dead console.
- `list_boot_profiles()`: shows known device profiles (builtin: `generic`,
  `u-boot-any-key`).
- `set_boot_credentials(profile_key, username, password)`: stores login
  credentials locally for a profile.

**Profiles** (shareable) vs **credentials** (local-only): profiles describe the
*recipe* — interrupt keys, pacing, reboot command, prompt regexes, and a
`login: {username, password_ref}` reference. Secrets never live in profiles.
Credentials are stored at `~/.config/serial-mcp/credentials.json` (mode 0600,
outside any repo) and are redacted from logs and `view_io` transcripts.

Custom profiles go in `~/.config/serial-mcp/profiles.json`:

```json
{
  "mydevice": {
    "description": "Vendor X board",
    "bootloader_prompt_regex": "(U-Boot|uboot)[^\n]*[>#]\s|^\s*=>\s",
    "abort_regex": "(login:|Login:)",
    "interrupt": { "keys": [" "], "delay": 0.2, "interval": 0.05, "max_attempts": 40 },
    "reboot": { "command": "reboot", "cycles": 3 },
    "login": { "username": "root", "password_ref": "mydevice" }
  }
}
```

Then: `set_boot_credentials("mydevice", "root", "…")` once, and the AI can
just call `enter_bootloader(profile="mydevice")`.

## Binary file transfer

File transfers use `upload_file` and `download_file`; the local side is always a
filesystem path visible to the MCP server process, so file bytes do not need to
be pasted into model context. Query `file_transfer_capabilities()` before using
a target. Transfers are bounded to 8 MiB by default and at most 8 MiB per call;
the overall timeout is bounded to 1–1800 seconds. Direct connections may retain
their server-owned socket/pty clients: the server pauses device RX and gates its
own client writers during the transfer. Attached sessions are also supported with
`confirm_shared_quiescence=true`, but tio's raw socket protocol has no arbitration
or client-identity frame. Before confirming, ensure every human/other client will
remain idle for the entire transfer. A conflicting client may corrupt the session;
Linux verification prevents installing mismatched files, and U-Boot upload verifies
RAM readback, but competing input cannot be prevented or guaranteed recoverable.
Transfer bytes are suppressed from logs and `view_io`; own-session users receive
an out-of-band notice and keystrokes during the gated interval are discarded.

### Linux shell

Requires an interactive POSIX shell with `cat`, `base64 -d`, and either a working
`sha256sum` or `openssl dgst -sha256`, reached through direct ownership or an
attached tio session (with shared-client quiescence confirmed). `sha256sum` is
preferred; OpenSSL is used only if sha256sum is absent or fails its known-output
probe. Each tool is selected only after its digest and exit status are checked.

**How the target shell is driven.** Interactive shells echo and wrap input and
can behave differently when a line is long, so the protocol keeps every control
line short:

- Every control line is at most 240 bytes and is refused before anything is
  sent if it would be longer. The directory and file names are set once in
  `SMC_*` shell variables (`SMC_D`, `SMC_T`, `SMC_B`, `SMC_F`), so lines do not
  repeat long paths. For the coordinator's `/tmp/serial-mcp-live-*.bin` path the
  longest control line measured 201 bytes in a longer scratch directory.
- Each line reports its exit status as `RC:<n>` and ends with a marker printed
  through `printf '%s'`, so the echoed command never contains the marker.
- Before any transfer step the shell must show its primary prompt. A continuation
  prompt (`> `) means input is still pending: only Ctrl-C is sent. EOF (Ctrl-D)
  is sent only into a `cat` this server started, never into a shell.
- Upload stages: prompt sync, capability probe, `cat` ready marker, streamed
  base64 (256-character lines, echo drained between lines), EOF, decode, length
  check, SHA-256 check, install (`ln` for no-clobber, `mv -f` for overwrite), and
  cleanup. Nothing is installed unless the staged file's length and digest match.
  A failure before install leaves the destination untouched and removes only the
  two staging names. A failure after install begins is reported as UNKNOWN.
- Download reads size, digest, and base64 in separate short lines, then verifies
  length and SHA-256 locally before atomically creating or replacing the file.

Failure messages name the stage, the number of command bytes, and a redacted RX
tail (base64-looking runs are replaced by `<b64-redacted>`), so diagnostics never
include file content.

```text
upload_file(local_path="./firmware.bin", target="linux",
            target_path="/tmp/firmware.bin", overwrite=false)
download_file(local_path="./capture.bin", target="linux",
              target_path="/tmp/capture.bin")
```

**Requirements and limits.** The shell must be at its prompt, and nobody should
type during the transfer. For an attached tio session, pass
`confirm_shared_quiescence=true`; the raw socket cannot enforce this. Linux
tests run a real `/bin/sh` (dash) over PTYs and a tio-like socket, including
OpenSSL-only and constrained-PATH targets, paced links, and interrupted streams.
BusyBox ash was not available on the test host, so ash line-editing limits remain
unverified. Staging cleanup is best effort: if the shell cannot be returned to its
primary prompt, the response names the staging files so an operator can remove
them after pressing Ctrl-C in tio.

### U-Boot RAM

U-Boot is not treated as a filesystem. Upload requires the target's `loady`
command compiled with YMODEM/CRC support; it transfers to a **required explicit
RAM address** and verifies the bytes by reading the range back with `md.b`.
`target_path` is only the simple ASCII YMODEM transfer filename. Before choosing
an address, the operator must confirm the range is writable and does not overlap
U-Boot, its stack, relocation area, or other live memory. No address is guessed.
Download uses `md.b <address> <length>` and needs both the explicit address and
byte length. `md.b` must produce standard address-colon hexadecimal rows, and a
U-Boot prompt regex must be supplied or configured (anchor it to the end of the
prompt, e.g. `U-Boot>\\s*$`). Upload verification is exact YMODEM packet CRC plus
byte-for-byte RAM readback. Download verification is limited to parsed sequential
addresses and exact byte count; `md.b` has no end-to-end checksum, so corrupted
text that remains syntactically plausible cannot be ruled out.

```text
upload_file(local_path="./blob.bin", target="uboot", target_path="blob.bin",
            address="0x82000000", prompt_regex="U-Boot>\\s*$")
download_file(local_path="./ram.bin", target="uboot", address="0x82000000",
              length=4096, prompt_regex="U-Boot>\\s*$")
```

The address in these examples is illustrative only; select a safe range for the
specific board. `loady` changes volatile RAM only. For attach-mode U-Boot, pass
`confirm_shared_quiescence=true` and ensure other tio clients stay idle; interrupted
uploads can leave partial volatile RAM changed. The tools do not write flash,
execute memory, reboot, or change persistent U-Boot environment variables. A
U-Boot build without `loady` cannot upload via this implementation; there is no
assumed generic U-Boot serial-send command. Use a target-specific supported
loader or a different physical transport if `loady` is absent.

### Support matrix and limits

| Target | Upload | Download | Requirements | Verification |
|---|---|---|---|---|
| Linux shell | base64 to temp then install | target base64 to local temp | `cat`, `base64 -d`, working `sha256sum` or `openssl dgst -sha256`; direct ownership or attached session with confirmed idle co-users | SHA-256 + exact length both ways |
| U-Boot | YMODEM via `loady` to explicit RAM address | `md.b` of explicit RAM address/length | YMODEM-enabled `loady`, `md.b`, explicit range, prompt regex; attached sessions require idle co-users | upload readback is byte-exact; download only address/length |
| Attach/shared session | Supported with `confirm_shared_quiescence=true` | Supported with `confirm_shared_quiescence=true` | Raw tio socket has no arbitration; all other clients must remain idle | protocol checks detect many collisions; interleaving is not prevented |

The U-Boot examples also work in attach mode by adding `confirm_shared_quiescence=true`.
`overwrite` applies to the Linux target for uploads and to the local destination
for downloads. It does not apply to U-Boot RAM: upload replaces the selected
volatile range, so select it deliberately. Transfer data itself is never added
to serial logs or the I/O transcript. The user still grants the agent access to
the named local file and target path/address; review paths, target capabilities,
and memory ranges before invoking a transfer.

## Shared daemon (protocol v2)

Protocol v2 is a breaking multi-device IPC contract. The Rust MCP frontend and
native terminal both speak v2. See [docs/daemon-ipc.md](docs/daemon-ipc.md) for
the versioned wire contract and supported global/device tool scope.

`serial-mcp-daemon` (`python -m serial_mcp_daemon`) owns zero or more physical
serial devices and shares them among local AI and human clients over a
permission-restricted UNIX socket. Each physical tty has independent membership,
output cursors, settings, operations, and an idle control lease. Device aliases
resolve to the same tty identity. Runtime socket discovery uses
`$SERIAL_MCP_RUNTIME_DIR`, else `$XDG_RUNTIME_DIR/serial-mcp`, else
`/tmp/serial-mcp-<uid>`.

The daemon, Rust MCP frontend and native human terminal speak protocol v2. The
explicit in-process legacy server is separate. The v2 contract and Python wire
helpers (`serial_mcp_ipc`) are documented in
[docs/daemon-ipc.md](docs/daemon-ipc.md).

- A registered client alone holds no device open. Membership joins keep a device
  open until the last participant leaves; active work settles before closure.
- `connect(port, baudrate)` opens/joins and requests control. First opener chooses
  baud; later joins use the actual rate. Busy joins become read-only observers.
  Only explicit `connect` acquires or reacquires control; modifications never
  auto-acquire. Control expires after 60 idle seconds without ending membership.
- Human clients join/leave without acquiring AI control. Human input is ungated,
  and per-client output reads do not consume another client's cursor.
- Every registered client keeps the daemon alive. When no clients remain, it
  waits 30 seconds before shutdown.

Frontends should use `serial_mcp_ipc` (`ensure_daemon`, `DaemonClient`), which is
stdlib-only.

## Human terminal

`serial-mcp-terminal` is an install-time-built Rust multi-device TUI for humans. It shows available and open devices in a left sidebar, keeps MCP-only count/status visible, and renders device ANSI output inside a VT100 screen buffer. Ctrl-] moves focus between the pane and the sidebar without changing membership, so it never invents an output gap; opening a different device leaves the prior membership and retains its cached screen with an observation-gap marker. A real gap is cleared only by an explicit `a` acknowledgement. The TUI never acquires AI control. Esc leaves the current device, Ctrl-B makes confirmed shared baud changes, and `q` or Ctrl-Q from the sidebar quits. Install and usage details, lifecycle semantics and accepted concurrent-input risks are in
[docs/human-terminal.md](docs/human-terminal.md).

## Legacy engine session surfaces

The legacy in-process engine/server can still expose a tio-compatible socket and
companion pty for its own direct-connect mode. The supported shared-daemon v2
connection deliberately creates neither surface; human applications join through
daemon IPC instead.

For the legacy engine only, one owner and any number of clients join over a UNIX
socket speaking tio's `--socket` protocol (raw bytes, no handshake or framing).

**When the legacy server owns the device** it may serve that socket and expose a companion pty:

- **Companion pty** (`/tmp/serial-mcp-<dev>`, a symlink to a virtual tty): what
  `tio`, `screen`, `minicom`, `cat` attach to.
- **UNIX socket** (`/tmp/serial-mcp-<dev>.sock`): the same raw protocol, for
  `nc -UN`, scripts, and other agents.

Device output is broadcast to every attached client, and every client's bytes are
forwarded to the device, so both directions are bridged across both surfaces.

**When this server attaches** to someone else's session (your tio), it is simply
another client on that socket: it holds no port, serves nothing, and creates no
pty. Detaching closes only its own connection.

Nobody's typing is echoed by the server; each party sees input via the device's
echo, exactly like tio's shared-session mode. Connection loss is surfaced as an
EIO error — the device was unplugged (direct mode), or the owner's socket closed
(attach mode). The reader stops and `connect` must be called again.

A liveness probe explains one wrinkle of attach mode: to tell a live session from
a leftover socket file, the server connects briefly and closes. The owner sees
that as a client that joined and left immediately, which its accept loop must
tolerate — tio does.

## Buffer semantics

The background reader continuously appends received bytes to an internal buffer
and logs RX/TX activity to `/tmp/serial-mcp.log`. `send_command` does not clear
pre-existing buffered data; it consumes and returns only data received after the
send point. Pre-existing unsolicited data remains available to `read_output`.
