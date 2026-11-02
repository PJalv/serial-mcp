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


def test_rust_terminal_counts_mcp_and_quits_cleanly_in_isolated_daemon(tmp_path,terminal_binary):
    runtime = tmp_path / "runtime"
    runtime.mkdir(mode=0o700)
    daemon = serial_mcp_daemon.DaemonServer(
        runtime, state=serial_mcp_engine.SerialState(log_path=str(tmp_path / "daemon.log"))
    )
    daemon.start()
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    env = dict(os.environ, SERIAL_MCP_RUNTIME_DIR=str(runtime))
    proc = subprocess.Popen([str(terminal_binary)], stdin=slave, stdout=slave, stderr=slave, env=env)
    os.close(slave)
    output = bytearray()
    mcp = ipc.DaemonClient.connect(runtime / "daemon.sock")
    try:
        mcp.hello("mcp", "synthetic-idle-client")
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


def test_rust_terminal_ctrl_bracket_returns_visible_focus_to_sidebar(tmp_path, monkeypatch, terminal_binary):
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
    observer.hello("mcp", "ctrl-bracket-status-observer")
    try:
        _read_until(master, output, b"MCPclients:")
        _read_until(master, output, port.encode(), timeout=5)
        os.write(master, b"\r115200\r")
        _read_until(master, output, b"connected", timeout=8)
        assert observer.status()["devices"][0]["members"] == 1

        os.write(master, b"\x16")
        _read_until(master, output, b"Next key sent literally", timeout=5)
        os.write(master, b"\x1d")
        readable, _, _ = select.select([device_master], [], [], 3)
        assert readable and os.read(device_master, 1) == b"\x1d"
        assert observer.status()["devices"][0]["members"] == 1
        while select.select([device_master], [], [], 0)[0]:
            os.read(device_master, 4096)

        leave_at = len(output)
        os.write(master, b"\x1d")
        _read_until(master, output, b"OUTPUT GAP", timeout=5, start=leave_at)
        deadline = time.monotonic() + 3
        while observer.status()["devices"] and time.monotonic() < deadline:
            time.sleep(0.01)
        assert observer.status()["devices"] == []

        os.write(master, b"\x1b[B")
        input_at = len(output)
        os.write(master, b"Z")
        readable, _, _ = select.select([device_master], [], [], 0.5)
        try:
            forwarded = os.read(device_master, 4096) if readable else b""
        except OSError:
            forwarded = b""  # The daemon closed its synthetic slave after leave.
        assert forwarded == b"", f"sidebar input reached serial PTY: {forwarded!r}"
        assert b"Z" not in _visible(output[input_at:])

    finally:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=5)
        observer.close()
        daemon.stop("test complete")
        os.close(master)
        os.close(device_master)


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

    finally:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=5)
        observer.close()
        daemon.stop("test complete")
        os.close(master)
        os.close(device_master)
