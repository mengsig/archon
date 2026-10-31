#!/usr/bin/env python3
"""Drive a command on a pseudo-terminal for simulate-update.sh.

`launch`: wait for the update question, answer it, then wait for the terminal
UI (the alternate screen) and quit it. `interrupt`: wait for a progress line,
send Ctrl+C, and time the exit. Every output line goes to the transcript with
its time in seconds since the reference moment (the answer, or the start);
a JSON result is written to --result. Exit status: 0 when every step was seen.
"""

import argparse
import fcntl
import json
import os
import pty
import select
import signal
import struct
import sys
import termios
import time

ALT_SCREEN = b"\x1b[?1049h"
CTRL_C = b"\x03"
CTRL_D = b"\x04"
ROWS, COLS = 40, 120


class Session:
    """A child on a pseudo-terminal, its output and the transcript."""

    def __init__(self, argv, transcript_path):
        assert argv, "a command to run"
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execvp(argv[0], argv)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.start = time.monotonic()
        self.reference = self.start
        self.output = b""
        self.pending = b""
        self.status = None
        self.transcript = open(transcript_path, "w", encoding="utf-8")
        self.note(f"started {' '.join(argv)}; times are seconds since this start, "
                  "and since the answer once one is sent")

    def elapsed(self):
        return time.monotonic() - self.reference

    def note(self, text):
        self.transcript.write(f"{self.elapsed():9.3f} ## {text}\n")
        self.transcript.flush()

    def record(self, data):
        self.output += data
        self.pending += data
        *lines, self.pending = self.pending.split(b"\n")
        for line in lines:
            shown = line.decode("utf-8", "replace").replace("\x1b", "\\e").replace("\r", "")
            self.transcript.write(f"{self.elapsed():9.3f} | {shown}\n")
        self.transcript.flush()

    def pump(self, timeout):
        """Read what arrives within `timeout`; False once the child has exited."""
        wait = timeout if self.status is None else 0
        ready, _, _ = select.select([self.fd], [], [], wait)
        if ready:
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                data = b""
            if data:
                self.record(data)
                return True
            time.sleep(0.05)
        return self.reap() is None

    def reap(self):
        if self.status is None:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid == self.pid:
                self.status = os.waitstatus_to_exitcode(status)
                self.note(f"exited with {self.status}")
        return self.status

    def wait_for(self, needle, limit, since=0):
        """Seconds until `needle` appears in output after offset `since`, or None."""
        deadline = time.monotonic() + limit
        while needle not in self.output[since:]:
            left = deadline - time.monotonic()
            if left <= 0 or not self.pump(min(left, 0.2)):
                return self.elapsed() if needle in self.output[since:] else None
        return self.elapsed()

    def wait_exit(self, limit):
        deadline = time.monotonic() + limit
        while self.reap() is None and time.monotonic() < deadline:
            self.pump(0.1)
        return self.status

    def send(self, data, label):
        self.note(f"sent {label}")
        os.write(self.fd, data)

    def kill(self):
        if self.reap() is None:
            self.note("killing the child")
            os.kill(self.pid, signal.SIGKILL)
            self.wait_exit(5)
        if self.pending:
            self.record(b"\n")
        self.transcript.close()


def run_launch(session, args, result):
    asked = session.wait_for(args.question.encode(), args.question_timeout)
    result["question_seen"] = asked is not None
    if asked is None:
        result["failure"] = f"no `{args.question}` within {args.question_timeout}s"
        return
    mark = len(session.output)
    session.reference = time.monotonic()
    session.send(args.answer.encode() + b"\r", f"answer {args.answer!r}")
    opened = session.wait_for(ALT_SCREEN, args.tui_timeout, since=mark)
    result["tui_after_s"] = opened
    if opened is None:
        result["failure"] = f"no terminal UI within {args.tui_timeout}s of the answer"
        return
    session.wait_for(b"\x00never", args.settle)
    for key, label in ((CTRL_D, "Ctrl+D"), (b"/quit\r", "/quit"), (CTRL_C, "Ctrl+C")):
        session.send(key, label)
        if session.wait_exit(args.quit_timeout) is not None:
            result["quit_with"] = label
            return
    result["failure"] = "the terminal UI did not quit on Ctrl+D, /quit or Ctrl+C"


def run_interrupt(session, args, result):
    seen = session.wait_for(args.trigger.encode(), args.trigger_timeout)
    result["trigger_seen_s"] = seen
    if seen is None:
        result["failure"] = f"no `{args.trigger}` within {args.trigger_timeout}s"
        return
    sent = time.monotonic()
    session.send(CTRL_C, "Ctrl+C")
    status = session.wait_exit(args.exit_limit)
    if status is None:
        result["failure"] = f"still running {args.exit_limit}s after Ctrl+C"
        session.wait_exit(args.grace)
        status = session.status
    result["exit_after_s"] = None if status is None else round(time.monotonic() - sent, 3)


def parse():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["launch", "interrupt"])
    parser.add_argument("--transcript", required=True)
    parser.add_argument("--result", required=True)
    parser.add_argument("--question", default="Update and restart now? [Y/n/s]")
    parser.add_argument("--answer", default="y")
    parser.add_argument("--question-timeout", type=float, default=60)
    parser.add_argument("--tui-timeout", type=float, default=60)
    parser.add_argument("--settle", type=float, default=2)
    parser.add_argument("--quit-timeout", type=float, default=5)
    parser.add_argument("--trigger", default="archon: update: asking archond to restart")
    parser.add_argument("--trigger-timeout", type=float, default=120)
    parser.add_argument("--exit-limit", type=float, default=3)
    parser.add_argument("--grace", type=float, default=30)
    if "--" not in sys.argv:
        parser.error("give the command after --")
    split = sys.argv.index("--")
    args = parser.parse_args(sys.argv[1:split])
    args.argv = sys.argv[split + 1:]
    if not args.argv:
        parser.error("give the command after --")
    return args


def main():
    args = parse()
    session = Session(args.argv, args.transcript)
    result = {"mode": args.mode, "argv": args.argv}
    try:
        (run_launch if args.mode == "launch" else run_interrupt)(session, args, result)
    finally:
        session.kill()
    result["exit_status"] = session.status
    with open(args.result, "w", encoding="utf-8") as out:
        json.dump(result, out, indent=2)
    return 1 if "failure" in result else 0


if __name__ == "__main__":
    sys.exit(main())
