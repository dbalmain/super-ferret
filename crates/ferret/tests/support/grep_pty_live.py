"""Feed stdin past the stream threshold, then one marker line, and report
whether the marker reached a real terminal while stdin was still open."""
import os
import select
import subprocess
import sys
import threading
import time

master, slave = os.openpty()
assert os.isatty(slave)
child = subprocess.Popen(sys.argv[1:], stdin=subprocess.PIPE,
                         stdout=slave, stderr=subprocess.DEVNULL)
os.close(slave)


def feed():
    line = b"hit " + b"x" * 59 + b"\n"
    try:
        child.stdin.write(line * ((5 << 20) // len(line)) + b"hit MARKER\n")
        child.stdin.flush()
    except BrokenPipeError:
        pass


threading.Thread(target=feed, daemon=True).start()
tail = b""
seen = False
deadline = time.monotonic() + 10
# A line the program withholds would block a plain read until stdin closed,
# and stdin closes only after this loop: poll instead.
while time.monotonic() < deadline:
    if not select.select([master], [], [], 0.1)[0]:
        continue
    try:
        chunk = os.read(master, 65536)
    except OSError:
        break
    if not chunk:
        break
    tail = (tail + chunk)[-64:]
    if b"MARKER" in tail:
        # stdin stays open until this loop ends, so any marker seen here
        # arrived first.
        seen = True
        break
if not seen:
    # The feeder may still be blocked writing; killing the child frees it,
    # so closing stdin cannot wait on its lock.
    child.kill()
try:
    child.stdin.close()
except BrokenPipeError:
    pass
# A child that hangs after stdin closes would hold the pty open forever:
# drain against a deadline, then kill it.
deadline = time.monotonic() + 10
while time.monotonic() < deadline:
    if not select.select([master], [], [], 0.1)[0]:
        continue
    try:
        if not os.read(master, 65536):
            break
    except OSError:
        break
child.kill()
child.wait(timeout=10)
print("seen" if seen else "withheld")
