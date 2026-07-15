#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
"""Type keystrokes into the dockur Windows VM via QEMU's HMP monitor socket.

The dockur container exposes no exec channel into the guest, but QEMU's
monitor does `sendkey` — enough to type a bootstrap one-liner into the
auto-logged-in desktop (Win+R → command → Enter) and stand up the share-polling
agent (agent.ps1). Assumes an en-US guest keyboard layout; stick to
layout-stable characters (letters, digits, space, dash, dot, slash) in
anything typed blind — bootstrap.ps1's marker files are the verification.

Usage:
    sudo sendkeys.py --type 'powershell -ep bypass ...'   # type a string
    sudo sendkeys.py --combo meta_l-r                     # one chord (Win+R)
    sudo sendkeys.py --combo ret --combo alt-y            # several chords
    sudo sendkeys.py --hmp 'info status'                  # raw HMP command

Needs root (the socket lives in the container's mount namespace, reached via
/proc/<qemu-pid>/root/dev/shm/monitor.sock).
"""

import argparse
import socket
import subprocess
import sys
import time

# ASCII → QEMU key name (en-US layout). Uppercase/shifted handled below.
PLAIN = {
    " ": "spc", "-": "minus", "=": "equal", "[": "bracket_left",
    "]": "bracket_right", ";": "semicolon", "'": "apostrophe",
    "`": "grave_accent", "\\": "backslash", ",": "comma", ".": "dot",
    "/": "slash",
}
SHIFTED = {
    "!": "1", "@": "2", "#": "3", "$": "4", "%": "5", "^": "6", "&": "7",
    "*": "8", "(": "9", ")": "0", "_": "minus", "+": "equal",
    "{": "bracket_left", "}": "bracket_right", ":": "semicolon",
    '"': "apostrophe", "~": "grave_accent", "|": "backslash", "<": "comma",
    ">": "dot", "?": "slash",
}


def qemu_monitor_path() -> str:
    pid = subprocess.run(
        ["pgrep", "-f", "qemu-system"], capture_output=True, text=True
    ).stdout.split()
    if not pid:
        sys.exit("no qemu-system process found (is the xbox-win container up?)")
    return f"/proc/{pid[0]}/root/dev/shm/monitor.sock"


class Monitor:
    def __init__(self) -> None:
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.settimeout(5)
        self.s.connect(qemu_monitor_path())
        time.sleep(0.2)
        self._drain()

    def _drain(self) -> str:
        out = b""
        self.s.setblocking(False)
        try:
            while True:
                chunk = self.s.recv(65536)
                if not chunk:
                    break
                out += chunk
        except BlockingIOError:
            pass
        self.s.setblocking(True)
        return out.decode(errors="replace")

    def cmd(self, line: str, settle: float = 0.06) -> str:
        self.s.send(line.encode() + b"\n")
        time.sleep(settle)
        return self._drain()

    def key(self, combo: str) -> None:
        self.cmd(f"sendkey {combo}")

    def type_string(self, text: str) -> None:
        for ch in text:
            if ch.isalpha():
                name = ch.lower()
                self.key(f"shift-{name}" if ch.isupper() else name)
            elif ch.isdigit():
                self.key(ch)
            elif ch in PLAIN:
                self.key(PLAIN[ch])
            elif ch in SHIFTED:
                self.key(f"shift-{SHIFTED[ch]}")
            else:
                sys.exit(f"no key mapping for character {ch!r}")


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--type", help="type a string")
    ap.add_argument("--combo", action="append", default=[],
                    help="send a raw sendkey chord (e.g. meta_l-r, ret, alt-y)")
    ap.add_argument("--hmp", help="send a raw HMP monitor command and print reply")
    ap.add_argument("--delay", type=float, default=0.0,
                    help="seconds to sleep first (queue behind a prior action)")
    args = ap.parse_args()

    if args.delay:
        time.sleep(args.delay)
    mon = Monitor()
    if args.hmp:
        print(mon.cmd(args.hmp, settle=0.3))
    for combo in args.combo:
        mon.key(combo)
        time.sleep(0.15)
    if args.type:
        mon.type_string(args.type)


if __name__ == "__main__":
    main()
