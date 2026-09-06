"""Process fixtures for testing which terminal owns an agent report."""

import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import subprocess
import termios
import time


def run_in_terminal(command, **kwargs):
    master, slave = pty.openpty()

    def attach():
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

    try:
        return subprocess.run(command, preexec_fn=attach, **kwargs)
    finally:
        os.close(slave)
        os.close(master)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--orphan", action="store_true")
    parser.add_argument("result", type=Path)
    parser.add_argument("input", type=json.loads)
    parser.add_argument("command", type=json.loads)
    args = parser.parse_args()

    if args.orphan:
        parent = os.getpid()
        if os.fork():
            return
        os.setsid()
        deadline = time.monotonic() + 3
        while os.getppid() == parent and time.monotonic() < deadline:
            time.sleep(0.01)
        assert os.getppid() != parent, "reporter was not reparented"

    result = subprocess.run(
        args.command,
        input=args.input,
        text=True,
        capture_output=True,
        start_new_session=True,
        timeout=3,
    )
    temporary = args.result.with_suffix(".tmp")
    temporary.write_text(json.dumps([result.returncode, result.stdout, result.stderr]))
    temporary.rename(args.result)


if __name__ == "__main__":
    main()
