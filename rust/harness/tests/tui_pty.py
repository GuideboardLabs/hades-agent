#!/usr/bin/env python3
"""Exercise the built TUI in a Linux PTY, offline. Usage: python3 tui_pty.py /path/to/hades"""
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

binary = str(Path(sys.argv[1]).resolve())
with tempfile.TemporaryDirectory(prefix="hades-tui-pty-") as scratch:
    env = dict(os.environ, HADES_HOME=scratch, TERM="xterm-256color", COLORTERM="truecolor")
    non_tty = subprocess.run([binary, "tui", "--demo"], capture_output=True, env=env, timeout=10)
    assert non_tty.returncode == 2 and b"interactive terminal" in non_tty.stderr
    master, slave = pty.openpty()
    before = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 52, 140, 0, 0))

    def control_terminal():
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

    process = subprocess.Popen([binary, "chat", "--demo", "--root", scratch], stdin=slave, stdout=slave, stderr=slave, env=env, preexec_fn=control_terminal)
    output = bytearray()

    def drain(seconds=0.35):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            if select.select([master], [], [], 0.04)[0]:
                try:
                    output.extend(os.read(master, 65536))
                except OSError:
                    break

    def send(data):
        os.write(master, data)
        drain()

    try:
        drain(1)
        assert b"HADES AGENT" in output and b"DEMO DATA" in output
        send(b"\x10usage\r")  # palette -> usage
        send(b"\x1b")
        send(b"\x1b[200~a pasted task\nwith Unicode: \xe7\x81\xab\x1b[201~")
        send(b"\r")
        send(b"\x1bOP")  # F1
        send(b"\x1b[6~")
        send(b"\x1b")
        for height, width in [(18, 56), (10, 20), (40, 100), (52, 140)]:
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
            os.kill(process.pid, signal.SIGWINCH)
            drain(0.2)
        for _ in range(7):
            send(b"\t")
        send(b"\x11")  # Ctrl+Q
        process.wait(timeout=10)
        drain(0.1)
        assert process.returncode == 0, output[-2000:]
        assert b"\x1b[?1049h" in output and b"\x1b[?1049l" in output
        assert b"\x1b[?2004h" in output and b"\x1b[?2004l" in output
        assert termios.tcgetattr(slave) == before, "terminal attributes were not restored"
        assert not list(Path(scratch).iterdir()), "offline demo unexpectedly wrote state"
        print("PTY checks passed: palette, paste, views, resize, clean exit, terminal restoration, no demo writes")
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)
        os.close(slave)
