"""A fake full-screen agent for stitching tests: a transcript in the alternate
screen, scrolled by the mouse wheel, with a status row at the bottom.

  --lines N        transcript length
  --header         a pinned header row while scrolled up
  --scrollbar      a scrollbar in the last column
  --loading        draw a "loading" frame before each real one
  --sync           wrap each frame in synchronized output
  --slow MS        wait before redrawing after a wheel notch
  --chatty         keep changing the title, so output never goes quiet
  --no-mouse       never ask for mouse reports
  --working        report status working instead of idle
  --blink MS       repaint a clock on the status row
  --fixed          ignore wheel scrolling, like a permission prompt
  --blocked        report a permission prompt
  --error          report status error
"""
import os, re, sys, termios, time, tty, select

args = sys.argv[1:]
flag = lambda name: name in args
def value(name, default):
    return int(args[args.index(name) + 1]) if name in args else default

lines = [f"line {i:03}" for i in range(1, value("--lines", 50) + 1)]
slow = value("--slow", 0) / 1000
cols, rows = os.get_terminal_size()
view = rows - 1  # the last row is the status row
offset = 0  # rows scrolled up from the bottom
wheels = 0
keys = ""
blink = value("--blink", 0) / 1000
last_blink = time.monotonic()

def out(s):
    os.write(1, s.encode())

def frame():
    top = max(0, len(lines) - view - offset)
    shown = lines[top:top + view]
    if flag("--header") and offset > 0 and top > 0:  # covers a row, but not at the very top
        shown = ["== pinned header =="] + shown[1:]
    body = []
    for i, text in enumerate(shown):
        row = text.ljust(cols - 1)[: cols - 1]
        if flag("--scrollbar"):
            thumb = int((top / max(1, len(lines) - view)) * (view - 3))
            row += "┃" if thumb <= i < thumb + 3 else "│"
        body.append(row)
    status = f"status wheels={wheels} keys={keys}"
    if flag("--blocked"):
        status += " approve shell? [Yes] No"
    if blink:
        status += f" clock={time.monotonic():.2f}"
    return "\x1b[H\x1b[2J" + "\r\n".join(body) + f"\x1b[{rows};1H" + status

def draw():
    if flag("--loading"):
        out("\x1b[H\x1b[2J" + "loading...")
        time.sleep(0.03)
    f = frame()
    out("\x1b[?2026h" + f + "\x1b[?2026l" if flag("--sync") else f)

fd = sys.stdin.fileno()
saved = termios.tcgetattr(fd)
tty.setraw(fd)
out("\x1b[?1049h")
if not flag("--no-mouse"):
    out("\x1b[?1000h\x1b[?1006h")
state = "idle"
if flag("--working"):
    state = "working"
elif flag("--blocked"):
    state = "blocked:kind=permission"
elif flag("--error"):
    state = "error"
out("\x1b]7501;state=" + state + "\x1b\\")
draw()
pending = b""
try:
    while True:
        ready, _, _ = select.select([fd], [], [], 0.004 if flag("--chatty") else 0.02)
        if flag("--chatty"):
            out(f"\x1b]2;tick {time.time()}\x07")
        if blink and time.monotonic() - last_blink >= blink:
            last_blink = time.monotonic()
            draw()
        if not ready:
            continue
        pending += os.read(fd, 1024)
        while pending:
            m = re.match(rb"\x1b\[<(\d+);\d+;\d+[Mm]", pending)
            if m:
                pending = pending[m.end():]
                button = int(m.group(1))
                if button in (64, 65):
                    wheels += 1
                    step = 1 if button == 64 else -1
                    if not flag("--fixed"):
                        offset = min(max(0, offset + step), max(0, len(lines) - view))
                    time.sleep(slow)
                    draw()
                continue
            if pending.startswith(b"\x1b") and len(pending) < 12 and b"M" not in pending and b"m" not in pending:
                break  # wait for the rest of a sequence
            ch, pending = pending[:1], pending[1:]
            if ch == b"q":
                raise SystemExit
            keys += ch.decode(errors="replace")
            draw()
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, saved)
