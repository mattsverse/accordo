#!/usr/bin/env python3
"""Exercise Accordo in a real Unix terminal; requires only Python's standard library."""

import ast
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time


BINARY = Path(os.environ.get("ACCORDO_BINARY", Path(__file__).resolve().parents[1] / "target/debug/accordo"))


def terminal_host():
    # Keep the controlling session alive after Accordo exits: macOS revokes the
    # slave when its session leader exits, making subsequent tcgetattr fail.
    process = subprocess.Popen([str(BINARY)])
    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGWINCH):
        signal.signal(signum, lambda number, _frame: process.send_signal(number))
    code = process.wait()
    Path("terminal-state").write_text(repr(termios.tcgetattr(0)))
    sys.exit(code)


def exercise(exit_method):
    with tempfile.TemporaryDirectory(prefix="accordo-terminal-") as temporary:
        directory = Path(temporary)
        config = "layout: tabbed\nservices:\n"
        for name in ("first", "second"):
            autostart = "true" if exit_method != "q" else "false"
            config += (
                f"  - name: {name}\n    autostart: {autostart}\n    cmd: |\n"
                f"      trap 'rm -f {name}.pid; exit 0' TERM; "
                f"echo $$ > {name}.pid; echo start >> {name}.runs; "
                f"echo {name.upper()}_READY; while :; do sleep 0.05; done\n"
            )
        (directory / "accordo.yaml").write_text(config)
        master, slave = pty.openpty()
        before = termios.tcgetattr(slave)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
        child = subprocess.Popen(
            [sys.executable, str(Path(__file__).resolve()), "--host"], cwd=directory,
            stdin=slave, stdout=slave, stderr=slave,
            env=os.environ | {"TERM": "xterm-256color"},
            start_new_session=True,
        )
        transcript = bytearray()

        def pump():
            if select.select([master], [], [], 0.02)[0]:
                try:
                    transcript.extend(os.read(master, 65536))
                except OSError:
                    pass

        def wait_for(predicate, description, seconds=8):
            deadline = time.monotonic() + seconds
            while not predicate():
                pump()
                if time.monotonic() >= deadline:
                    raise AssertionError(f"Timed out: {description}; Accordo status={child.poll()}")

        def settle():
            deadline = time.monotonic() + 0.15
            while time.monotonic() < deadline:
                pump()

        def runs(name):
            path = directory / f"{name}.runs"
            return len(path.read_text().splitlines()) if path.exists() else 0

        def press(keys):
            os.write(master, keys)

        try:
            wait_for(lambda: b"start/stop" in transcript, "initial screen")
            assert not termios.tcgetattr(slave)[3] & termios.ICANON
            if exit_method == "q":
                assert runs("first") == runs("second") == 0
                # Alternate-screen terminals often translate the wheel into
                # arrow keys unless the application requests mouse reports.
                mouse_reporting = b"\x1b[?1006h" in transcript and any(
                    mode in transcript for mode in (b"\x1b[?1000h", b"\x1b[?1002h", b"\x1b[?1003h")
                )
                press(b"\x1b[<64;60;10M" if mouse_reporting else b"\x1b[A")
                press(b"s")
                wait_for(lambda: runs("first") + runs("second") == 1, "start service after scrolling logs")
                assert runs("first") == 1, "Scrolling inside the log view changed the selected service"
                settle()
                press(b"r")
                wait_for(lambda: runs("first") == 2, "restart first service")
                settle()
                press(b"s")
                wait_for(lambda: not (directory / "first.pid").exists(), "stop first service")
                settle()
                press(b"1s")
                wait_for(lambda: runs("second") == 1, "number key selects second service")
                settle()
                press(b"\x1b[As")
                wait_for(lambda: runs("first") == 3, "up arrow selects first service")
                settle()
                for rows, columns in [(2, 8), (30, 100)]:
                    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
                    os.kill(child.pid, signal.SIGWINCH)
                    settle()
                press(b"\x1b[Br")
                wait_for(lambda: runs("second") == 2, "down arrow and restart after resize")
                settle()
                press(b"q")
                expected = 0
            elif exit_method == "ctrl-c":
                wait_for(lambda: runs("first") == runs("second") == 1, "autostart")
                press(b"\x03")
                expected = 130
            else:
                wait_for(lambda: runs("first") == runs("second") == 1, "autostart")
                os.kill(child.pid, signal.SIGTERM)
                expected = 143

            wait_for(lambda: child.poll() is not None, f"exit via {exit_method}")
            settle()
            assert child.returncode == expected, (exit_method, child.returncode)
            after = ast.literal_eval((directory / "terminal-state").read_text())
            assert after == before, "Terminal settings were not restored"
            assert b"\x1b[?1049l" in transcript, "Alternate screen was not restored"
            assert b"\x1b[?1006l" in transcript, "Mouse reporting was not disabled on exit"
            assert not list(directory.glob("*.pid")), "Services survived shutdown"
            print(f"PASS: terminal {exit_method}, service cleanup, and terminal restoration")
        finally:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
            for path in directory.glob("*.pid"):
                try:
                    os.killpg(int(path.read_text()), signal.SIGKILL)
                except (ProcessLookupError, ValueError):
                    pass
            os.close(master)
            os.close(slave)


if __name__ == "__main__":
    if sys.argv[1:] == ["--host"]:
        terminal_host()
    for method in ("q", "ctrl-c", "sigterm"):
        exercise(method)
