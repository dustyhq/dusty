"""Draw the pixel-art "dusty" logo with dust blowing off the letters.

python3 scripts/make-logo.py docs/media/dusty-logo.svg           -> the animated logo for dark pages
python3 scripts/make-logo.py docs/media/dusty-logo-light.svg light -> the same for light pages
python3 scripts/make-logo.py frame.svg dark 8                    -> frame 8 frozen, for checking

The colours follow the editor's rule for the dashboard logo: letters in the text colour, and
the dust and shadow in shades of the accent colour (see LogoColors in crates/dusty-ui).
"""
import random
import sys

LETTERS = {
    "d": [".....##", ".....##", ".....##", ".######", "##...##", "##...##",
          "##...##", "##...##", ".######"],
    "u": ["", "", "", "##...##", "##...##", "##...##", "##...##", "##...##", ".######"],
    "s": ["", "", "", ".######", "##.....", ".#####.", ".....##", ".....##", "######."],
    "t": ["", ".##...", ".##...", "######", ".##...", ".##...", ".##...", ".##...", "..####"],
    "y": ["", "", "", "##...##", "##...##", "##...##", "##...##", ".######", ".....##",
          ".....##", "######."],
}
WORD = "dusty"
GAP = 2
TOP = 14          # rows of sky above the tallest letter
LEFT = 13         # matches the room on the right, so the word sits in the middle
FRAMES = 24       # one loop
FRAME_MS = 110

# (page, text, accent) for each kind of page: Dusty's own theme, and GitHub's light one.
PAGES = {
    "dark": ("#0d1117", "#c9d1d9", "#58a6ff"),
    "light": ("#ffffff", "#1f2328", "#0969da"),
}


def rgb(color):
    return tuple(int(color[i:i + 2], 16) for i in (1, 3, 5))


def shade(color, toward, percent):
    mixed = (((a * (100 - percent) + b * percent) // 100) for a, b in zip(rgb(color), rgb(toward)))
    return "#" + "".join(f"{part:02x}" for part in mixed)


page_kind = sys.argv[2] if len(sys.argv) > 2 else "dark"
PAGE, TEXT, ACCENT = PAGES[page_kind]
INK = TEXT
SHADOW = shade(ACCENT, PAGE, 65)
# Dust from pale to dark.
DUST = [shade(ACCENT, TEXT, 50), ACCENT, shade(ACCENT, PAGE, 25), shade(ACCENT, PAGE, 45)]

letters = set()
x = LEFT
for ch in WORD:
    rows = LETTERS[ch]
    width = max(len(r) for r in rows)
    for y, row in enumerate(rows):
        for dx, c in enumerate(row):
            if c == "#":
                letters.add((x + dx, TOP + y))
    x += width + GAP
WIDTH = x - GAP + 1 + LEFT  # the letters, their shadow, and LEFT on each side
HEIGHT = TOP + 12

# The top pixel of every letter column: dust settles there and the wind lifts it off.
column_top = {}
for (px, py) in letters:
    column_top[px] = min(py, column_top.get(px, py))
sources = sorted(column_top.items())

rng = random.Random(5)

# Settled dust: a thin layer along the letter tops, with a few small drifts.
settled = []
layer = {}
for px, py in sources:
    h = 2 if rng.random() < 0.25 else 1
    layer[px] = py - h
    for k in range(1, h + 1):
        color = DUST[3] if k == 1 and rng.random() < 0.5 else DUST[2]
        settled.append((px, py - k, color, False))

# Blowing dust: grains lift off the layer and stream up and to the right in the wind.
grains = []
lifting = [(px, top) for px, top in layer.items() if px in column_top and column_top[px] <= TOP + 4]
for i in range(150):
    sx, sy = rng.choice(lifting)
    life = rng.randint(8, 17)
    start = rng.randrange(FRAMES)
    path = [(sx, sy)]
    for step in range(life - 1):
        gx, gy = path[-1]
        gx += 1 if rng.random() < 0.75 else 0
        gy -= 1 if rng.random() < (0.7 if step < 3 else 0.4) else 0
        path.append((gx, gy))
    grains.append((start, path, None))


def grain_at(grain, frame):
    start, path, color = grain
    age = (frame - start) % FRAMES
    if age >= len(path):
        return None
    fade = age / len(path)
    opacity = 1.0 if fade < 0.5 else round(1.0 - (fade - 0.5) * 1.6, 2)
    color = color or DUST[max(0, 3 - int(fade * 4))]
    return path[age], color, opacity


def rect(x, y, color, extra=""):
    return f'<rect x="{x}" y="{y}" width="1" height="1" fill="{color}"{extra}/>'


def static(frame):
    out = []
    for (px, py) in sorted(letters):
        out.append(rect(px + 1, py + 1, SHADOW))
    for (px, py) in sorted(letters):
        out.append(rect(px, py, INK))
    for (sx, sy, color, flicker) in settled:
        if flicker and (frame + sx) % 6 < 2:
            continue
        out.append(rect(sx, sy, color))
    for grain in grains:
        at = grain_at(grain, frame)
        if at:
            (gx, gy), color, opacity = at
            out.append(rect(gx, gy, color, f' opacity="{opacity}"'))
    return out


def animated():
    out = []
    style = [
        "rect{shape-rendering:crispEdges}",
        f".f{{animation:f {6 * FRAME_MS}ms step-end infinite}}",
        "@keyframes f{0%{opacity:1}66.6%{opacity:0}}",
    ]
    for (px, py) in sorted(letters):
        out.append(rect(px + 1, py + 1, SHADOW))
    for (px, py) in sorted(letters):
        out.append(rect(px, py, INK))
    for (sx, sy, color, flicker) in settled:
        delay = -((sx % 6) * FRAME_MS)
        extra = f' class="f" style="animation-delay:{delay}ms"' if flicker else ""
        out.append(rect(sx, sy, color, extra))
    total = FRAMES * FRAME_MS
    for i, grain in enumerate(grains):
        start, path, color = grain
        x0, y0 = path[0]
        steps = []
        last = None
        for frame in range(FRAMES):
            at = grain_at(grain, frame)
            pct = round(100 * frame / FRAMES, 3)
            if at:
                (gx, gy), fill, opacity = at
                state = f"transform:translate({gx - x0}px,{gy - y0}px);fill:{fill};opacity:{opacity}"
            else:
                state = "opacity:0"
            # With step-end timing a keyframe only matters where the state changes.
            if state != last:
                steps.append(f"{pct}%{{{state}}}")
                last = state
        style.append(f"@keyframes g{i}{{{''.join(steps)}}}")
        style.append(f".g{i}{{animation:g{i} {total}ms step-end infinite}}")
        out.append(rect(x0, y0, DUST[3], f' class="g{i}" opacity="0"'))
    return "<style>" + "".join(style) + "</style>" + "".join(out)


def svg(body):
    scale = 9
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {WIDTH} {HEIGHT}" '
        f'width="{WIDTH * scale}" height="{HEIGHT * scale}" shape-rendering="crispEdges">'
        f'<title>dusty</title>{body}</svg>\n'
    )


path = sys.argv[1]
if len(sys.argv) > 3:
    body = "".join(static(int(sys.argv[3])))
else:
    body = animated()
with open(path, "w") as f:
    f.write(svg(body))
