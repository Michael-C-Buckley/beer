#!/usr/bin/env python3
"""Probe the kitty keyboard protocol by printing the raw bytes each key sends.

Run it inside a terminal to see what that terminal emits under a chosen set of
progressive-enhancement flags:

    python3 scripts/kitty-probe.py [flags]

`flags` is the decimal flag set to enable (default 1):

    1   disambiguate escape codes
    2   report event types (press/repeat/release)
    4   report alternate keys
    8   report all keys as escape codes
    16  report associated text

Combine by adding, e.g. 15 for everything but text, 31 for everything. Press
Esc to quit; the previous keyboard mode is restored on exit.
"""

import os
import sys
import termios
import tty

flags = int(sys.argv[1]) if len(sys.argv) > 1 else 1
fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
try:
    tty.setraw(fd)
    os.write(1, f"\033[>{flags}u".encode())  # push current flags and enable
    os.write(1, f"flags={flags}; keys print as raw bytes, Esc quits\r\n".encode())
    while True:
        b = os.read(fd, 64)
        os.write(1, (repr(b) + "\r\n").encode())
        if b == b"\x1b" or b"27u" in b:  # Esc in legacy or CSI u form
            break
finally:
    os.write(1, b"\033[<u")  # pop back to the previous mode
    termios.tcsetattr(fd, termios.TCSADRAIN, old)
