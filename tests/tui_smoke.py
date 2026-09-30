"""Exercise the actual v1 terminal application on a Unix PTY; no Python packages needed."""

import fcntl
import os
from pathlib import Path
import pty
import re
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time


def exercise(binary, path, expected, interact=False):
    master, slave = pty.openpty()
    control_read, control_write = os.pipe()
    before = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))

    def attach_terminal():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    process = subprocess.Popen(
        # Keep a session owner alive after the application exits. macOS hangs up
        # the slave PTY when its session owner exits, preventing termios checks.
        [sys.executable, "-c",
         "import os, subprocess, sys; "
         "code = subprocess.run(sys.argv[2:], timeout=30).returncode; "
         "os.write(1, ('\\x1b]777;exit=%d\\x07' % code).encode()); "
         "os.read(int(sys.argv[1]), 1); sys.exit(code)",
         str(control_read), binary, "tui", str(path)],
        stdin=slave, stdout=slave, stderr=slave,
        cwd=path.parent,
        env={**os.environ, "TERM": "xterm-256color"},
        preexec_fn=attach_terminal,
        pass_fds=(control_read,),
    )
    output = bytearray()

    def result():
        match = re.search(rb"\x1b\]777;exit=(\d+)\x07", output)
        return int(match[1]) if match else None

    def pump(seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            if select.select([master], [], [], min(0.05, max(0, deadline - time.monotonic())))[0]:
                output.extend(os.read(master, 65536))
            if process.poll() is not None:
                break

    try:
        deadline = time.monotonic() + 15
        while result() is None and process.poll() is None and b"Cycle graph" not in output and time.monotonic() < deadline:
            pump(0.1)
        if interact:
            assert b"Cycle graph" in output, "analysis did not finish in the terminal"
            # Graph labels exercise UTF-8 truncation, then switch local/global views.
            for keys in [b"5", b"v", b"v", b"t", b"+", b"\x1b"]:
                os.write(master, keys)
                pump(0.3)
                assert result() is None and process.poll() is None, "TUI exited during graph navigation"
            # Source can change after analysis: opening an old span must not panic.
            path.write_text("", encoding="utf-8")
            for keys in [b"1", b"\r", b"\x1b", b"5", b"/"]:
                os.write(master, keys)
                pump(0.3)
                assert result() is None and process.poll() is None, "TUI exited while opening source/search"
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 2, 5, 0, 0))
            pump(0.3)
            os.write(master, b"\x03")  # Ctrl-C also exits from an overlay.
        deadline = time.monotonic() + 5
        while result() is None and process.poll() is None and time.monotonic() < deadline:
            pump(0.1)
        assert result() == expected, f"unexpected TUI exit: {result()}"
        assert b"panicked" not in output, "TUI panicked"
        after = termios.tcgetattr(slave)
        # BSD can set PENDIN when restoring canonical input. It is transient
        # pending-input state, not a mode set by the application.
        before[3] &= ~getattr(termios, "PENDIN", 0)
        after[3] &= ~getattr(termios, "PENDIN", 0)
        assert after == before, f"terminal mode was not restored: {[(i, a, b) for i, (a, b) in enumerate(zip(before, after)) if a != b]}"
        assert b"\x1b[?1049l" in output, "alternate screen was not restored"
        os.write(control_write, b"x")
        assert process.wait(timeout=5) == expected
    finally:
        if process.poll() is None:
            process.kill()
        process.wait()
        os.close(master)
        os.close(slave)
        os.close(control_read)
        os.close(control_write)


def main():
    binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/pymapper-v1").resolve())
    with tempfile.TemporaryDirectory(prefix="pymapper-tui-") as directory:
        path = Path(directory) / "example.py"
        path.write_text(
            "\n\n\ndef 数据处理流程示例函数():\n    return 1\n"
            "\ndef main():\n    return 数据处理流程示例函数()\n",
            encoding="utf-8",
        )
        exercise(binary, path, 0, interact=True)
        exercise(binary, Path(directory) / "missing.py", 1)
        path.write_text("def broken(:\n", encoding="utf-8")
        exercise(binary, path, 1)
    print("TUI smoke passed: graph navigation, Unicode, changed source, resize, failure exits, terminal restoration")


if __name__ == "__main__":
    main()
