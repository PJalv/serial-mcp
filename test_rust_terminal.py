"""Rust terminal smoke tests against isolated daemon runtimes and synthetic PTYs."""
__test__ = False  # Collected through the legacy test_terminal module path.
import contextlib
import fcntl
import os
import re
import struct
import termios
import pty
import select
import signal
import subprocess
import time
from pathlib import Path

import pytest

import serial_mcp_daemon
import serial_mcp_engine
import serial_mcp_ipc as ipc
import serial_mcp_terminal

ROOT = Path(__file__).resolve().parent
BINARY = ROOT / "build" / "rust-target" / "release" / "serial-mcp-terminal"


@pytest.fixture(scope="session")
def terminal_binary():
    subprocess.run(["cargo","build","--release","--locked","--bins","--manifest-path",str(ROOT/"rust-client"/"Cargo.toml"),"--target-dir",str(ROOT/"build"/"rust-target")],cwd=ROOT,check=True)
    assert BINARY.is_file(), "cargo succeeded without producing the native terminal"
    return BINARY


def test_editable_terminal_launcher_resolves_site_packages_binary(tmp_path,monkeypatch):
    module=tmp_path/"source"/"serial_mcp_terminal.py"
    module.parent.mkdir()
    module.write_text("")
    binary=tmp_path/"site-packages"/"serial-mcp-terminal"
    binary.parent.mkdir()
    binary.write_bytes(b"synthetic executable")
    binary.chmod(0o755)
    monkeypatch.setattr(serial_mcp_terminal,"__file__",str(module))
    monkeypatch.setattr(serial_mcp_terminal.sysconfig,"get_paths",lambda:{"purelib":str(binary.parent)})
    monkeypatch.setattr(serial_mcp_terminal.sys,"argv",["/synthetic/bin/serial-mcp-terminal","--help"])
    executed=[]
    monkeypatch.setattr(serial_mcp_terminal.os,"execv",lambda path,args:executed.append((path,args)))
    serial_mcp_terminal.main()
    assert executed==[(str(binary),[str(binary),"--help"])]


def test_editable_terminal_launcher_uses_source_build_binary(tmp_path,monkeypatch):
    module=tmp_path/"source"/"serial_mcp_terminal.py"
    module.parent.mkdir()
    module.write_text("")
    binary=module.parent/"build"/"rust-target"/"release"/"serial-mcp-terminal"
    binary.parent.mkdir(parents=True)
    binary.write_bytes(b"synthetic executable")
    binary.chmod(0o755)
    monkeypatch.setattr(serial_mcp_terminal,"__file__",str(module))
    monkeypatch.setattr(serial_mcp_terminal.sysconfig,"get_paths",lambda:{"purelib":str(tmp_path/"empty-site")})
    monkeypatch.setattr(serial_mcp_terminal.sys,"argv",["/synthetic/bin/serial-mcp-terminal"])
    executed=[]
    monkeypatch.setattr(serial_mcp_terminal.os,"execv",lambda path,args:executed.append((path,args)))
    serial_mcp_terminal.main()
    assert executed==[(str(binary),[str(binary)])]


def _visible(buf):
    return re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", bytes(buf))


def _screen(buf, rows=30, cols=100):
    """Replay cursor-positioning output into a text screen.

    Ratatui repaints only changed cells, so a focus-marker word can be split
    across escape sequences. Rendering the stream into its final grid keeps the
    assertions about visible UI text stable.
    """
    import unicodedata

    text = bytes(buf).decode("utf-8", "ignore")
    grid = [[" "] * cols for _ in range(rows)]
    row = col = 0
    i = 0
    while i < len(text):
        ch = text[i]
        if ch == "\x1b":
            if i + 1 < len(text) and text[i + 1] == "[":
                j = i + 2
                while j < len(text) and not ("@" <= text[j] <= "~"):
                    j += 1
                if j >= len(text):
                    break
                params = text[i + 2 : j].lstrip("?")
                final = text[j]
                numbers = [int(p) if p.isdigit() else 0 for p in params.split(";")] if params else []
                if final in "Hf":
                    row = max(0, (numbers[0] if numbers and numbers[0] else 1) - 1)
                    col = max(0, (numbers[1] if len(numbers) > 1 and numbers[1] else 1) - 1)
                elif final == "A":
                    row = max(0, row - (numbers[0] if numbers and numbers[0] else 1))
                elif final == "B":
                    row = min(rows - 1, row + (numbers[0] if numbers and numbers[0] else 1))
                elif final == "C":
                    col = min(cols, col + (numbers[0] if numbers and numbers[0] else 1))
                elif final == "D":
                    col = max(0, col - (numbers[0] if numbers and numbers[0] else 1))
                elif final == "K":
                    mode = numbers[0] if numbers else 0
                    if row < rows and mode == 2:
                        grid[row] = [" "] * cols
                    elif row < rows and mode == 0:
                        for c in range(min(col, cols), cols):
                            grid[row][c] = " "
                elif final == "J":
                    mode = numbers[0] if numbers else 0
                    if row < rows and mode == 0:
                        for c in range(min(col, cols), cols):
                            grid[row][c] = " "
                    if mode in (0, 2) and row + 1 < rows:
                        for r in range(row + 1, rows):
                            grid[r] = [" "] * cols
                i = j + 1
                continue
            i += 2
            continue
        if ch == "\r":
            col = 0
        elif ch == "\n":
            row = min(rows - 1, row + 1)
        elif ch >= " ":
            width = 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1
            if row < rows and col < cols:
                grid[row][col] = ch
                if width == 2 and col + 1 < cols:
                    grid[row][col + 1] = ""
            col += width
            if col >= cols:
                col = 0
                row = min(rows - 1, row + 1)
        i += 1
    return "\n".join("".join(r).rstrip() for r in grid)


def _pump(fd, buf, seconds):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        readable, _, _ = select.select([fd], [], [], 0.05)
        if readable:
            try:
                buf.extend(os.read(fd, 65536))
            except OSError:
                return
        else:
            time.sleep(0.01)


def _pane_focus(text):
    """The pane holds focus, whichever line carries the marker."""
    return "focus:pane" in text or "Terminal · FOCUS" in text


def _sidebar_focus(text):
    """The sidebar holds focus; a gap/baud prompt replaces the header line."""
    return (
        "focus:sidebar" in text
        or "Devices · FOCUS" in text
        or "Sidebar focus" in text
    )


def _open_selected(fd, buf, rows=30, cols=100, baud=b"115200"):
    """Press Enter on the selected device, answering a baud prompt if asked."""
    os.write(fd, b"\r")
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        _pump(fd, buf, 0.1)
        text = _screen(buf, rows, cols)
        if _pane_focus(text):
            return text
        if "Baud rate 1" in text or "Baud:" in text:
            os.write(fd, baud + b"\r")
        if "OUTPUT GAP" in text and ("Connected" in text or "control" in text):
            return text
    return _wait_screen(fd, buf, _pane_focus, timeout=5, rows=rows, cols=cols)


def _wait_screen(fd, buf, predicate, timeout=5, rows=30, cols=100):
    """Wait until the rendered screen satisfies predicate, then return its text."""
    deadline = time.monotonic() + timeout
    text = _screen(buf, rows, cols)
    while not predicate(text) and time.monotonic() < deadline:
        _pump(fd, buf, 0.1)
        text = _screen(buf, rows, cols)
    assert predicate(text), text[-1600:]
    return text


def _read_until(fd, buf, needle, timeout=5, start=0):
    deadline = time.monotonic() + timeout
    while needle not in _visible(buf[start:]) and time.monotonic() < deadline:
        readable, _, _ = select.select([fd], [], [], 0.1)
        if readable:
            try:
                buf.extend(os.read(fd, 8192))
            except OSError:
                break
    assert needle in _visible(buf[start:]), _visible(buf[-2000:])


def _read_raw_until(fd, buf, needle, timeout=5, start=0):
    deadline = time.monotonic() + timeout
    while needle not in buf[start:] and time.monotonic() < deadline:
        readable, _, _ = select.select([fd], [], [], 0.1)
        if readable:
            try:
                buf.extend(os.read(fd, 8192))
            except OSError:
                break
    assert needle in buf[start:], repr(buf[-2000:])


def test_rust_terminal_counts_mcp_and_quits_cleanly_in_isolated_daemon(tmp_path,terminal_binary):
    runtime = tmp_path / "runtime"
    runtime.mkdir(mode=0o700)
    daemon = serial_mcp_daemon.DaemonServer(
        runtime, state=serial_mcp_engine.SerialState(log_path=str(tmp_path / "daemon.log"))
    )
    daemon.start()
    mcp = ipc.DaemonClient.connect(runtime / "daemon.sock")
    mcp.hello("mcp", "synthetic-idle-client")
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    env = dict(os.environ, SERIAL_MCP_RUNTIME_DIR=str(runtime))
    proc = subprocess.Popen([str(terminal_binary)], stdin=slave, stdout=slave, stderr=slave, env=env)
    os.close(slave)
    output = bytearray()
    try:
        _read_until(master, output, b"MCPclients:1")
        os.write(master, b"q")
        proc.wait(timeout=5)
        assert proc.returncode == 0
    finally:
        mcp.request("bye")
        mcp.close()
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=5)
        daemon.stop("test complete")
        os.close(master)


def test_rust_terminal_ansi_output_does_not_replace_persistent_chrome(tmp_path, monkeypatch,terminal_binary):
    runtime = tmp_path / "runtime"
    runtime.mkdir(mode=0o700)
    device_master, device_slave = pty.openpty()
    port = os.ttyname(device_slave)
    os.close(device_slave)
    monkeypatch.setattr(
        serial_mcp_engine.serial.tools.list_ports,
        "comports",
        lambda: [type("Port", (), {"device": port, "description": "synthetic", "hwid": ""})()],
    )
    daemon = serial_mcp_daemon.DaemonServer(
        runtime, state=serial_mcp_engine.SerialState(log_path=str(tmp_path / "daemon.log"))
    )
    daemon.start()
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    proc = subprocess.Popen(
        [str(terminal_binary)], stdin=slave, stdout=slave, stderr=slave,
        env=dict(os.environ, SERIAL_MCP_RUNTIME_DIR=str(runtime)),
    )
    os.close(slave)
    output = bytearray()
    observer=ipc.DaemonClient.connect(runtime / "daemon.sock")
    observer.hello("human","status-observer")
    try:
        _read_until(master, output, b"MCPclients:")
        # Select the synthetic port, enter a valid baud, then send ANSI that clears
        # the terminal buffer. The sidebar/header are rendered in a separate layer.
        os.write(master, b"\r115200\r")
        _read_until(master, output, b"connected", timeout=8)
        input_at=len(output)
        os.write(master,b"X")
        readable,_,_=select.select([device_master],[],[],3)
        assert readable and os.read(device_master,1)==b"X"
        assert b"X" not in _visible(output[input_at:])
        os.write(device_master, b"\x1b[2J\x1b[Hsynthetic-output")
        _read_until(master, output, b"synthetic-output", timeout=5)
        assert b"synthetic-output" in _visible(output)
        assert b"MCPclients:" in _visible(output)
        before=observer.status()["devices"]
        assert before and before[0]["members"]==1
        leave_at=len(output)
        os.write(master, b"\x1b")
        _read_until(master, output, b"OUTPUT GAP", timeout=5, start=leave_at)
        deadline=time.monotonic()+3
        while observer.status()["devices"] and time.monotonic()<deadline: time.sleep(0.01)
        assert observer.status()["devices"]==[]
        os.write(master, b"q")
        proc.wait(timeout=5)
    finally:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=5)
        observer.close()
        daemon.stop("test complete")
        os.close(master)
        os.close(device_master)


def _start_synthetic_terminal(tmp_path, monkeypatch, terminal_binary, count, size=(30,100)):
    """Start an isolated daemon plus the native TUI over synthetic device ptys."""
    import serial_mcp_engine

    runtime = tmp_path / "runtime"
    runtime.mkdir(mode=0o700)
    devices = []
    for _ in range(count):
        master, slave = pty.openpty()
        devices.append((master, os.ttyname(slave)))
        os.close(slave)
    monkeypatch.setattr(
        serial_mcp_engine.serial.tools.list_ports,
        "comports",
        lambda: [
            type("Port", (), {"device": port, "description": "synthetic", "hwid": ""})()
            for _, port in devices
        ],
    )
    daemon = serial_mcp_daemon.DaemonServer(
        runtime, state=serial_mcp_engine.SerialState(log_path=str(tmp_path / "daemon.log"))
    )
    daemon.start()
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", size[0], size[1], 0, 0))
    proc = subprocess.Popen(
        [str(terminal_binary)], stdin=slave, stdout=slave, stderr=slave,
        env=dict(os.environ, SERIAL_MCP_RUNTIME_DIR=str(runtime)),
    )
    os.close(slave)
    return runtime, daemon, proc, master, devices


def _stop_synthetic_terminal(daemon, proc, master, devices, clients=()):
    for client in clients:
        client.close()
    if proc.poll() is None:
        proc.terminate()
        proc.wait(timeout=5)
    daemon.stop("test complete")
    os.close(master)
    for device_master, _ in devices:
        os.close(device_master)


def _members(client, port):
    """Human+MCP membership count, or 0 when the daemon already closed the port."""
    for device in client.status()["devices"]:
        if device["port"] == port:
            return device["members"]
    return 0


def test_rust_terminal_ctrl_bracket_focus_keeps_membership_and_output(tmp_path, monkeypatch, terminal_binary):
    runtime, daemon, proc, master, devices = _start_synthetic_terminal(
        tmp_path, monkeypatch, terminal_binary, 1
    )
    device_master, port = devices[0]
    output = bytearray()
    observer = ipc.DaemonClient.connect(runtime / "daemon.sock")
    observer.hello("mcp", "focus-status-observer")
    try:
        _wait_screen(master, output, lambda t: port in t)
        os.write(master, b"\r115200\r")
        _wait_screen(master, output, _pane_focus, timeout=8)
        assert _members(observer, port) == 1

        # Ctrl-] only moves focus: membership and subscription must survive.
        os.write(master, b"\x1d")
        text = _wait_screen(master, output, _sidebar_focus)
        assert _members(observer, port) == 1
        assert "GAP" not in text

        # Output arriving while the sidebar is focused is still rendered.
        os.write(device_master, b"\r\nFOCUSED_SIDEBAR_OUTPUT")
        text = _wait_screen(
            master, output, lambda t: "FOCUSED_SIDEBAR_OUTPUT" in t, timeout=5
        )
        assert "GAP" not in text

        # Ctrl-] toggles back to the pane without a gap.
        os.write(master, b"\x1d")
        text = _wait_screen(master, output, _pane_focus)
        assert _members(observer, port) == 1
        assert "GAP" not in text

        # Direct device input still reaches the pane-focus path.
        os.write(master, b"Y")
        readable, _, _ = select.select([device_master], [], [], 3)
        assert readable and os.read(device_master, 1) == b"Y"

        # Quit is a sidebar action: return focus there first.
        os.write(master, b"\x1d")
        _wait_screen(master, output, _sidebar_focus)
        os.write(master, b"q")
        proc.wait(timeout=5)
        assert observer.status()["devices"] == []
    finally:
        _stop_synthetic_terminal(daemon, proc, master, devices, [observer])


def test_rust_terminal_sidebar_focus_routes_input_away_from_device(tmp_path, monkeypatch, terminal_binary):
    runtime, daemon, proc, master, devices = _start_synthetic_terminal(
        tmp_path, monkeypatch, terminal_binary, 1
    )
    device_master, port = devices[0]
    output = bytearray()
    observer = ipc.DaemonClient.connect(runtime / "daemon.sock")
    observer.hello("mcp", "focus-routing-observer")
    try:
        _wait_screen(master, output, lambda t: port in t)
        os.write(master, b"\r115200\r")
        _wait_screen(master, output, _pane_focus, timeout=8)
        os.write(master, b"\x1d")
        _wait_screen(master, output, _sidebar_focus)
        while select.select([device_master], [], [], 0)[0]:
            os.read(device_master, 4096)

        # Plain sidebar input is never forwarded to the device.
        screen_before = _screen(output)
        os.write(master, b"Z")
        _pump(master, output, 0.4)
        readable, _, _ = select.select([device_master], [], [], 0.2)
        assert not readable, "sidebar input reached the serial pty"
        assert _screen(output) == screen_before, "sidebar input changed the screen"

        # Navigation keeps focus in the sidebar and leaves membership intact.
        os.write(master, b"\x1b[B")
        _pump(master, output, 0.3)
        assert _members(observer, port) == 1
        while select.select([device_master], [], [], 0)[0]:
            os.read(device_master, 4096)

        # A sidebar mouse click selects without forwarding to the device.
        os.write(master, b"\x1b[<0;3;4M\x1b[<0;3;4m")
        _pump(master, output, 0.3)
        assert _members(observer, port) == 1
        readable, _, _ = select.select([device_master], [], [], 0.2)
        assert not readable, "sidebar mouse click reached the serial pty"

        # Re-entering the joined device returns to the pane without a gap.
        os.write(master, b"\r")
        text = _wait_screen(master, output, _pane_focus)
        assert _members(observer, port) == 1
        assert "GAP" not in text
        while select.select([device_master], [], [], 0)[0]:
            os.read(device_master, 4096)
        os.write(master, b"W")
        readable, _, _ = select.select([device_master], [], [], 3)
        assert readable and os.read(device_master, 1) == b"W"

        os.write(master, b"\x1d")
        _wait_screen(master, output, _sidebar_focus)
        os.write(master, b"q")
        proc.wait(timeout=5)
    finally:
        _stop_synthetic_terminal(daemon, proc, master, devices, [observer])


def test_rust_terminal_ctrl_bracket_literal_escape_still_sends_ctrl_bracket(tmp_path, monkeypatch, terminal_binary):
    runtime, daemon, proc, master, devices = _start_synthetic_terminal(
        tmp_path, monkeypatch, terminal_binary, 1
    )
    device_master, port = devices[0]
    output = bytearray()
    try:
        _wait_screen(master, output, lambda t: port in t)
        os.write(master, b"\r115200\r")
        _wait_screen(master, output, _pane_focus, timeout=8)
        while select.select([device_master], [], [], 0)[0]:
            os.read(device_master, 4096)

        os.write(master, b"\x16")
        _read_until(master, output, b"Next key sent", timeout=5)
        os.write(master, b"\x1d")
        readable, _, _ = select.select([device_master], [], [], 3)
        assert readable and os.read(device_master, 1) == b"\x1d"
        # The literal path must not move focus to the sidebar.
        assert not _sidebar_focus(_screen(output))

        os.write(master, b"\x16")
        _read_until(master, output, b"Next key sent", timeout=5)
        os.write(master, b"\x1b")
        readable, _, _ = select.select([device_master], [], [], 3)
        assert readable and os.read(device_master, 1) == b"\x1b"

        os.write(master, b"\x1d")
        _wait_screen(master, output, _sidebar_focus)
        os.write(master, b"q")
        proc.wait(timeout=5)
    finally:
        _stop_synthetic_terminal(daemon, proc, master, devices)


def test_rust_terminal_gap_acknowledged_explicitly_and_never_auto_cleared(tmp_path, monkeypatch, terminal_binary):
    runtime, daemon, proc, master, devices = _start_synthetic_terminal(
        tmp_path, monkeypatch, terminal_binary, 1
    )
    device_master, port = devices[0]
    output = bytearray()
    observer = ipc.DaemonClient.connect(runtime / "daemon.sock")
    observer.hello("mcp", "gap-status-observer")
    try:
        _wait_screen(master, output, lambda t: port in t)
        os.write(master, b"\r115200\r")
        _wait_screen(master, output, _pane_focus, timeout=8)
        os.write(device_master, b"\r\nKEEP_ME_CACHED")
        _wait_screen(master, output, lambda t: "KEEP_ME_CACHED" in t, timeout=5)

        # Esc really leaves, so this is a genuine gap.
        os.write(master, b"\x1b")
        text = _wait_screen(master, output, lambda t: "OUTPUT GAP" in t)
        assert _members(observer, port) == 0

        # Rejoining must not clear the marker on its own.
        text = _open_selected(master, output)
        assert "KEEP_ME_CACHED" in text
        assert "OUTPUT GAP" in text

        # Explicit acknowledgement clears the marker but keeps the scrollback.
        os.write(master, b"\x1d")
        _wait_screen(master, output, _sidebar_focus)
        os.write(master, b"a")
        text = _wait_screen(master, output, lambda t: "Gap acknowledged" in t)
        assert "KEEP_ME_CACHED" in text
        assert "OUTPUT GAP" not in text
        assert "lost output" in text

        # The acknowledgement is for that gap only; a new gap comes back.
        _open_selected(master, output)
        os.write(master, b"\x1b")
        _wait_screen(master, output, lambda t: "OUTPUT GAP" in t)
        # The cached scrollback survives both the acknowledgement and the new
        # gap; it is only rendered again while a pane is focused.
        text = _open_selected(master, output)
        assert "KEEP_ME_CACHED" in text
        assert "OUTPUT GAP" in text
        os.write(master, b"\x1d")
        _wait_screen(master, output, _sidebar_focus)

        os.write(master, b"q")
        proc.wait(timeout=5)
    finally:
        _stop_synthetic_terminal(daemon, proc, master, devices, [observer])


def test_rust_terminal_switching_devices_keeps_membership_and_gap_semantics(tmp_path, monkeypatch, terminal_binary):
    runtime, daemon, proc, master, devices = _start_synthetic_terminal(
        tmp_path, monkeypatch, terminal_binary, 2
    )
    by_port = {port: device_master for device_master, port in devices}
    output = bytearray()
    observer = ipc.DaemonClient.connect(runtime / "daemon.sock")
    observer.hello("mcp", "switch-status-observer")
    try:
        _wait_screen(
            master, output, lambda t: all(port in t for _, port in devices), timeout=8
        )

        def joined_port():
            for device in observer.status()["devices"]:
                if device["members"] == 1:
                    return device["port"]
            return None

        # Join whichever device the sidebar starts on.
        os.write(master, b"\r115200\r")
        _wait_screen(master, output, _pane_focus, timeout=8)
        first_port = joined_port()
        assert first_port in by_port, observer.status()
        first_master = by_port[first_port]
        second_port = next(port for port in by_port if port != first_port)
        second_master = by_port[second_port]

        os.write(first_master, b"\r\nFIRST_SCREEN")
        _wait_screen(master, output, lambda t: "FIRST_SCREEN" in t, timeout=5)

        # Sidebar navigation alone never detaches.
        os.write(master, b"\x1d")
        _wait_screen(master, output, _sidebar_focus)
        os.write(master, b"\x1b[B")
        _pump(master, output, 0.3)
        assert joined_port() == first_port

        # Opening the other device is what releases the first membership.
        os.write(master, b"\r")
        _wait_screen(master, output, lambda t: "Baud rate 1" in t, timeout=5)
        assert joined_port() == first_port
        os.write(master, b"115200\r")
        _wait_screen(master, output, _pane_focus, timeout=8)
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline and joined_port() != second_port:
            time.sleep(0.01)
        assert joined_port() == second_port, observer.status()
        os.write(second_master, b"\r\nSECOND_SCREEN")
        _wait_screen(master, output, lambda t: "SECOND_SCREEN" in t, timeout=5)

        # Returning marks the first with a genuine gap and keeps its cache.
        os.write(master, b"\x1d")
        _wait_screen(master, output, _sidebar_focus)
        os.write(master, b"\x1b[A")
        _pump(master, output, 0.2)
        text = _open_selected(master, output)
        assert "FIRST_SCREEN" in text
        assert "OUTPUT GAP" in text
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline and joined_port() != first_port:
            time.sleep(0.01)
        assert joined_port() == first_port, observer.status()

        os.write(master, b"\x1d")
        _wait_screen(master, output, _sidebar_focus)
        os.write(master, b"q")
        proc.wait(timeout=5)
    finally:
        _stop_synthetic_terminal(daemon, proc, master, devices, [observer])


def test_rust_terminal_focus_does_not_truncate_cached_screen(tmp_path, monkeypatch, terminal_binary):
    runtime, daemon, proc, master, devices = _start_synthetic_terminal(
        tmp_path, monkeypatch, terminal_binary, 1, size=(24, 50)
    )
    device_master, port = devices[0]
    output = bytearray()
    observer = ipc.DaemonClient.connect(runtime / "daemon.sock")
    observer.hello("mcp", "narrow-cache-observer")
    try:
        _wait_screen(master, output, lambda t: port in t, rows=24, cols=50)
        os.write(master, b"\r115200\r")
        _wait_screen(master, output, _pane_focus, timeout=8, rows=24, cols=50)

        # A wide line reaches the far right of the pane while focus is there.
        marker = b"RIGHTMARGIN_" + b"x" * 20
        os.write(device_master, b"\r\n" + marker)
        text = _wait_screen(
            master, output, lambda t: "RIGHTMARGIN_" in t, timeout=5, rows=24, cols=50
        )
        assert "x" * 20 in text

        # Focusing the narrow drawer must not truncate the cached cells, so the
        # right edge is still there when the pane comes back.
        os.write(master, b"\x1d")
        _wait_screen(master, output, _sidebar_focus, rows=24, cols=50)
        os.write(master, b"\x1d")
        text = _wait_screen(master, output, _pane_focus, rows=24, cols=50)
        assert "x" * 20 in text
        assert "GAP" not in text

        os.write(master, b"\x1d")
        _wait_screen(master, output, _sidebar_focus, rows=24, cols=50)
        os.write(master, b"q")
        proc.wait(timeout=5)
    finally:
        _stop_synthetic_terminal(daemon, proc, master, devices, [observer])


def test_rust_terminal_invalid_baud_does_not_detach_the_current_device(tmp_path, monkeypatch, terminal_binary):
    runtime, daemon, proc, master, devices = _start_synthetic_terminal(
        tmp_path, monkeypatch, terminal_binary, 2
    )
    by_port = {port: device_master for device_master, port in devices}
    output = bytearray()
    observer = ipc.DaemonClient.connect(runtime / "daemon.sock")
    observer.hello("mcp", "switch-baud-observer")
    try:
        _wait_screen(
            master, output, lambda t: all(port in t for _, port in devices), timeout=8
        )

        def joined_port():
            for device in observer.status()["devices"]:
                if device["members"] == 1:
                    return device["port"]
            return None

        os.write(master, b"\r115200\r")
        _wait_screen(master, output, _pane_focus, timeout=8)
        first_port = joined_port()
        assert first_port in by_port

        # Select the other device and enter an out-of-range baud. Nothing is joined,
        # so the existing membership must not be released.
        os.write(master, b"\x1d")
        _wait_screen(master, output, _sidebar_focus)
        os.write(master, b"\x1b[B")
        _pump(master, output, 0.2)
        os.write(master, b"\r")
        _wait_screen(master, output, lambda t: "Baud rate 1" in t, timeout=5)
        # An out-of-range value is rejected and the existing membership survives it.
        os.write(master, b"4000001\r")
        _wait_screen(master, output, lambda t: "Invalid baud rate" in t, timeout=5)
        assert joined_port() == first_port, observer.status()
        assert not [d for d in observer.status()["devices"] if d["state"] == "closing"]

        # The still-joined device keeps working: route input and output as before.
        os.write(master, b"\x1b[A")
        _pump(master, output, 0.2)
        os.write(master, b"\r")
        _wait_screen(master, output, _pane_focus, timeout=8)
        assert joined_port() == first_port
        first_master = by_port[first_port]
        while select.select([first_master], [], [], 0)[0]:
            os.read(first_master, 4096)
        os.write(master, b"K")
        readable, _, _ = select.select([first_master], [], [], 3)
        assert readable and os.read(first_master, 1) == b"K"

        os.write(master, b"\x1d")
        _wait_screen(master, output, _sidebar_focus)
        os.write(master, b"q")
        proc.wait(timeout=5)
    finally:
        _stop_synthetic_terminal(daemon, proc, master, devices, [observer])


def test_rust_terminal_resize_wider_keeps_cached_screen(tmp_path, monkeypatch, terminal_binary):
    runtime, daemon, proc, master, devices = _start_synthetic_terminal(
        tmp_path, monkeypatch, terminal_binary, 1, size=(24, 50)
    )
    device_master, port = devices[0]
    output = bytearray()
    try:
        _wait_screen(master, output, lambda t: port in t, rows=24, cols=50)
        os.write(master, b"\r115200\r")
        _wait_screen(master, output, _pane_focus, timeout=8, rows=24, cols=50)
        os.write(device_master, b"\r\nKEEPRIGHT_" + b"y" * 20)
        _wait_screen(master, output, lambda t: "KEEPRIGHT_" in t, timeout=5, rows=24, cols=50)

        # Between 50 and 63 columns the drawer hides the pane; widening past 64
        # brings the narrow sidebar back. Either way the pane width must end up
        # able to show the cached right edge, and no gap may be invented.
        os.write(master, b"\x1d")
        _wait_screen(master, output, _sidebar_focus, rows=24, cols=50)
        for cols in (64, 80, 100):
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 24, cols, 0, 0))
            os.kill(proc.pid, signal.SIGWINCH)
            _wait_screen(master, output, _sidebar_focus, rows=24, cols=cols, timeout=6)
        # Let any queued resize repaint settle before the focus keystroke.
        _pump(master, output, 0.5)
        os.write(master, b"\x1d")
        text = _wait_screen(
            master, output, _pane_focus, rows=24, cols=100, timeout=6
        )
        assert "y" * 20 in text, text
        assert "GAP" not in text

        os.write(master, b"\x1d")
        _wait_screen(master, output, _sidebar_focus, rows=24, cols=100)
        os.write(master, b"q")
        proc.wait(timeout=5)
    finally:
        _stop_synthetic_terminal(daemon, proc, master, devices)


def test_rust_terminal_narrow_drawer_focus_and_resize(tmp_path, monkeypatch, terminal_binary):
    runtime, daemon, proc, master, devices = _start_synthetic_terminal(
        tmp_path, monkeypatch, terminal_binary, 1, size=(24, 50)
    )
    device_master, port = devices[0]
    output = bytearray()
    observer = ipc.DaemonClient.connect(runtime / "daemon.sock")
    observer.hello("mcp", "narrow-status-observer")
    try:
        _wait_screen(master, output, lambda t: port in t, rows=24, cols=50)
        os.write(master, b"\r115200\r")
        _wait_screen(master, output, _pane_focus, timeout=8, rows=24, cols=50)

        # Ctrl-] brings the narrow drawer back with the device list visible.
        os.write(master, b"\x1d")
        text = _wait_screen(
            master, output, _sidebar_focus, rows=24, cols=50
        )
        assert "Devices" in text
        assert _members(observer, port) == 1

        os.write(master, b"\x1d")
        _wait_screen(master, output, _pane_focus, rows=24, cols=50)
        assert _members(observer, port) == 1

        # Resizing while focused keeps the app alive, joined and rendering.
        for rows, cols in [(30, 100), (20, 44), (24, 50)]:
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
            os.kill(proc.pid, signal.SIGWINCH)
            _wait_screen(
                master, output, _pane_focus, rows=rows, cols=cols, timeout=6
            )
        assert proc.poll() is None
        assert _members(observer, port) == 1
        os.write(device_master, b"\r\nAFTER_RESIZE")
        _wait_screen(master, output, lambda t: "AFTER_RESIZE" in t, timeout=5)

        os.write(master, b"\x1d")
        _wait_screen(master, output, _sidebar_focus, rows=24, cols=50)
        os.write(master, b"q")
        proc.wait(timeout=5)
    finally:
        _stop_synthetic_terminal(daemon, proc, master, devices, [observer])


def test_rust_terminal_shared_baud_prompts_are_visible(tmp_path, monkeypatch, terminal_binary):
    runtime = tmp_path / "runtime"
    runtime.mkdir(mode=0o700)
    device_master, device_slave = pty.openpty()
    port = os.ttyname(device_slave)
    os.close(device_slave)
    monkeypatch.setattr(
        serial_mcp_engine.serial.tools.list_ports,
        "comports",
        lambda: [type("Port", (), {"device": port, "description": "synthetic", "hwid": ""})()],
    )
    daemon = serial_mcp_daemon.DaemonServer(
        runtime, state=serial_mcp_engine.SerialState(log_path=str(tmp_path / "daemon.log"))
    )
    daemon.start()
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    proc = subprocess.Popen(
        [str(terminal_binary)], stdin=slave, stdout=slave, stderr=slave,
        env=dict(os.environ, SERIAL_MCP_RUNTIME_DIR=str(runtime)),
    )
    os.close(slave)
    output = bytearray()
    observer = ipc.DaemonClient.connect(runtime / "daemon.sock")
    observer.hello("mcp", "baud-status-observer")
    try:
        _read_until(master, output, b"MCPclients:")
        _read_until(master, output, port.encode(), timeout=5)
        os.write(master, b"\r115200\r")
        _read_until(master, output, b"connected", timeout=8)
        resize_at = len(output)
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 50, 0, 0))
        os.kill(proc.pid, signal.SIGWINCH)
        _read_until(master, output, b"Terminal", start=resize_at)
        confirm_at = len(output)
        os.write(master, b"\x02")
        _read_until(master, output, b"Shared baud affects all clients", start=confirm_at)
        _read_until(master, output, b"Confirm", start=confirm_at)
        _read_until(master, output, b"y/n", start=confirm_at)

        os.write(master, b"\x1b[200~y9600\r\x1b[201~")
        os.write(master, b"n")
        assert not select.select([device_master], [], [], 0)[0]
        devices = observer.status()["devices"]
        assert devices and devices[0]["baudrate"] == 115200

        confirm_at = len(output)
        os.write(master, b"\x02")
        _read_until(master, output, b"Confirm", start=confirm_at)
        prompt_at = len(output)
        os.write(master, b"y")
        _read_until(master, output, b"Baud rate 1", start=prompt_at)
        _read_until(master, output, b"confirms", start=prompt_at)
        _read_until(master, output, b"cancels", start=prompt_at)
        os.write(master, b"9600\r")
        deadline = time.monotonic() + 5
        devices = []
        while time.monotonic() < deadline:
            devices = observer.status()["devices"]
            if devices and devices[0]["baudrate"] == 9600:
                break
            time.sleep(0.01)
        assert devices and devices[0]["baudrate"] == 9600

        resize_at = len(output)
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 1, 10, 0, 0))
        os.kill(proc.pid, signal.SIGWINCH)
        _read_raw_until(master, output, b"\x1b[1;1H", start=resize_at)
        warning_at = len(output)
        os.write(master, b"\x02")
        _read_until(master, output, b"Resize", start=warning_at)
        devices = observer.status()["devices"]
        assert devices and devices[0]["baudrate"] == 9600

        resize_at = len(output)
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 50, 0, 0))
        os.kill(proc.pid, signal.SIGWINCH)
        _read_until(master, output, b"serial-mcp", start=resize_at)
        confirm_at = len(output)
        os.write(master, b"\x02")
        _read_until(master, output, b"Confirm", start=confirm_at)
        os.write(master, b"n")
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            devices = observer.status()["devices"]
            if devices and devices[0]["baudrate"] == 9600:
                break
            time.sleep(0.01)
        assert devices and devices[0]["baudrate"] == 9600

    finally:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=5)
        observer.close()
        daemon.stop("test complete")
        os.close(master)
        os.close(device_master)
