# Human terminal (`serial-mcp-terminal`)

`serial-mcp-terminal` is the install-time-built Rust multi-device TUI over daemon IPC v2 (Ratatui, Crossterm and a VT100 terminal buffer). It combines available ports and daemon-open devices in a left sidebar and keeps the MCP-only count and connection/actual baud/control status visible above an isolated ANSI terminal pane. The app never opens hardware itself or acquires AI control.

The TUI has two focus targets: the terminal pane and the device sidebar. **Ctrl-] moves focus between them and never changes daemon membership.** While the sidebar is focused the TUI stays joined and subscribed, so device output keeps arriving and no observation gap is invented. Opening a *different* device releases the previous membership as before; its cached screen stays local and its missing interval is marked as a real gap.

## Install and launch

```sh
uvx --from git+https://github.com/alvinla264/serial-mcp serial-mcp-terminal
serial-mcp-terminal
nix run .#terminal
```

Run from a real terminal (stdin must be a tty). The native executable is built at installation time; launch does not invoke Cargo or fall back to a Python TUI. The terminal connects as a human IPC client. Use Up/Down or j/k to choose a device, Enter to join, and `q` or Ctrl-Q to quit from the sidebar. A newly opened device prompts for a baud rate from 1 through 4,000,000; malformed values are rejected. An already-open device is joined at its actual baud without reconfiguration. The MCP count includes idle MCP registrations and excludes human clients. An older v2 daemon without MCP-only count telemetry is reported as unsupported rather than mislabelled. No client names/IDs or controller identity are displayed.

The header and pane/sidebar titles state which focus is active. In a narrow terminal the sidebar becomes a full-width drawer while it holds focus and the pane returns when focus moves back.

| Input | Action |
|---|---|
| Ctrl-] | Toggle focus between the terminal pane and the sidebar; never sent, never leaves |
| Up/Down, j/k | Select a device in the sidebar |
| Enter | Open the focused device: re-focus the pane if already joined, otherwise join (or prompt for baud) |
| Esc in pane | Leave the current device and focus the sidebar |
| `a` in sidebar | Acknowledge a real gap marker (keeps cached scrollback; does not restore lost output) |
| `d` in sidebar | Disconnect from the selected device |
| Ctrl-B in pane | Ask confirmation, then change shared baud; may disrupt active AI work |
| Ctrl-End in pane | Scroll to the newest output |
| Ctrl-V, then Ctrl-] or Esc | Send a literal Ctrl-] or Escape byte to the device |
| q or Ctrl-Q in sidebar | Quit; not sent |
| Other terminal input | Sent to the focused device without local echo, including Ctrl-C and arrows |
| Mouse click | Focus the sidebar and select a device |

## Focus, gaps and membership

Ctrl-] is a pure focus change: the TUI keeps its daemon membership and output subscription, so output that arrives while the sidebar is focused is still rendered into the cached screen. Sidebar navigation alone never detaches. Re-entering the device already joined does not leave, rejoin, or create a gap.

A genuine gap is recorded only when observation was really interrupted: leaving a device (explicitly, or by opening a different one), a device or daemon loss, a subscription failure, or a daemon `output_gap`/queue-overflow notice. A gap marker stays until it is acknowledged with `a`; rejoining never clears it silently, and acknowledging it never deletes cached scrollback or pretends the lost interval was captured.

The screen buffer is independent from the UI chrome, so device ANSI/cursor/clear sequences cannot erase the header or sidebar. Output history is bounded and memory-only. Cached screen contents remain local after leaving. Daemon/device loss is visible; recovery requires explicit reconnect. Writes with unknown outcomes are never replayed. The TUI restores terminal modes on exit and handled termination signals.

## Ownership and concurrent input

The daemon alone owns serial ports. The human TUI never acquires AI control. Human input can interleave with active AI operations, including file transfers; this accepted behavior is not prevented by the UI. Device status reports only factual transport/control state and does not infer login or device health. A human baud change requires an explicit confirmation because it affects all participants and may disrupt active AI work.

Leaving removes only this TUI's membership. Other participants keep the physical port open; after the last participant leaves, the daemon safely settles active work and closes it. Exiting removes the active membership and deregisters this client. Registration alone holds no device.

## Credential and transcript behavior

Human keystrokes are not masked. The daemon's `view_io` transcript may expose human TX bytes to MCP clients, and the daemon logger may record operation diagnostics. Do not type credentials unless their visibility to other authorized daemon clients is acceptable. Device output is delivered independently; one client's reads do not consume another client's output. The daemon IPC contract is documented in [daemon-ipc.md](daemon-ipc.md).
