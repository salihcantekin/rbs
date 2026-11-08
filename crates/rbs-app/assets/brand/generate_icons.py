#!/usr/bin/env python3
"""Regenerates the application icon files from one design description (see README.md).

The design is a list of primitives in a 256-unit box; the SVG master and every raster
are drawn from it, so they cannot drift apart. Needs Pillow; `rbs.icns` also needs macOS
`iconutil`. Run from this directory.
"""
import os, shutil, subprocess, tempfile
from PIL import Image, ImageDraw

SS = 4  # supersampling for the rasters

# --- Design (256-unit box) -------------------------------------------------------
TILE_TOP, TILE_BOT, RIM = "#141418", "#050507", "#3B3E4A"
RIM_W = 3
CORNER = 57
WHITE, LAV, MID, SIDE = "#F8F9FF", "#CFC6F5", "#9B86E6", "#E9E4FB"
ACCENT = "#5326D9"
RING_A, RING_B = "#FF9AD5", "#8B7BFF"
W, H = 218, 164
X, Y = 128 - W / 2, 128 - H / 2
BX, BY = X + W - 32, Y + H - 24  # camera bubble centre
CLIP_R = 18


def hex_rgb(h):
    return tuple(int(h[i:i + 2], 16) for i in (1, 3, 5))


def design():
    """Primitives drawn in order, inside the screen rectangle's clip unless noted."""
    side_w = 58
    body = [
        ("rect", X, Y, W, 30, LAV),
        ("circle", X + 18, Y + 15, 5, ACCENT, 1.0),
        ("circle", X + 34, Y + 15, 5, ACCENT, 0.6),
        ("circle", X + 50, Y + 15, 5, ACCENT, 0.35),
        ("rect", X, Y + 30, side_w, H - 30, SIDE),
        ("bar", X + 12, Y + 46, 34, 7, MID),
        ("bar", X + 12, Y + 62, 34, 7, LAV),
        ("bar", X + 12, Y + 78, 34, 7, LAV),
    ]
    cx, cy, cw = X + 76, Y + 48, 112
    body += [
        ("bar", cx, cy, cw * 0.55, 9, ACCENT),
        ("bar", cx, cy + 22, cw, 7, LAV),
        ("bar", cx, cy + 38, cw * 0.9, 7, LAV),
        ("bar", cx, cy + 54, cw * 0.7, 7, LAV),
    ]
    return body


# --- SVG -------------------------------------------------------------------------
def svg():
    def prim(p):
        k = p[0]
        if k == "rect":
            return f'<rect x="{p[1]:.1f}" y="{p[2]:.1f}" width="{p[3]:.1f}" height="{p[4]:.1f}" fill="{p[5]}"/>'
        if k == "bar":
            return f'<rect x="{p[1]:.1f}" y="{p[2]:.1f}" width="{p[3]:.1f}" height="{p[4]:.1f}" rx="{p[4] / 2:.1f}" fill="{p[5]}"/>'
        return f'<circle cx="{p[1]:.1f}" cy="{p[2]:.1f}" r="{p[3]}" fill="{p[4]}" opacity="{p[5]}"/>'
    body = "\n      ".join(prim(p) for p in design())
    person = (f'<circle cx="{BX}" cy="{BY - 14 * .8 + 2:.1f}" r="{13 * .8:.1f}" fill="#fff"/>'
              f'<path d="M{BX - 24 * .8:.1f} {BY + 30 * .8 + 2:.1f}a{24 * .8:.1f} {22 * .8:.1f} 0 0 1 {48 * .8:.1f} 0Z" fill="#fff"/>')
    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256">
  <!-- The application icon: a window on a black tile with a camera bubble. Fixed colours, the OS does not follow the theme. -->
  <defs>
    <linearGradient id="tile" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{TILE_TOP}"/><stop offset="1" stop-color="{TILE_BOT}"/></linearGradient>
    <linearGradient id="ring" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="{RING_A}"/><stop offset="1" stop-color="{RING_B}"/></linearGradient>
    <clipPath id="screen"><rect x="{X}" y="{Y}" width="{W}" height="{H}" rx="{CLIP_R}"/></clipPath>
  </defs>
  <rect width="256" height="256" rx="{CORNER}" fill="url(#tile)"/>
  <rect x="{RIM_W / 2}" y="{RIM_W / 2}" width="{256 - RIM_W}" height="{256 - RIM_W}" rx="{CORNER - RIM_W / 2}" fill="none" stroke="{RIM}" stroke-width="{RIM_W}"/>
  <rect x="{X}" y="{Y}" width="{W}" height="{H}" rx="{CLIP_R}" fill="{WHITE}"/>
  <g clip-path="url(#screen)">
      {body}
  </g>
  <circle cx="{BX}" cy="{BY}" r="41.5" fill="url(#ring)"/>
  <circle cx="{BX}" cy="{BY}" r="37" fill="{TILE_BOT}"/>
  {person}
</svg>
'''


# --- Raster ----------------------------------------------------------------------
def gradient(n, c1, c2, vertical, box=None):
    """An n x n gradient; `box` (x0, y0, x1, y1) is the area the gradient spans."""
    x0, y0, x1, y1 = box or (0, 0, n - 1, n - 1)
    g = Image.new("RGBA", (n, n)); px = g.load(); a, b = hex_rgb(c1), hex_rgb(c2)
    for y in range(n):
        for x in range(n):
            ty = min(max((y - y0) / max(y1 - y0, 1), 0), 1)
            tx = min(max((x - x0) / max(x1 - x0, 1), 0), 1)
            t = ty if vertical else (tx + ty) / 2
            px[x, y] = tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3)) + (255,)
    return g


def render(size, margin_share):
    """`size` px canvas; the 256-unit design fills it minus `margin_share` on each side."""
    n = size * SS
    inner = n * (1 - 2 * margin_share)
    k = inner / 256; off = n * margin_share
    u = lambda v: v * k + off
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))

    def rrect_mask(x0, y0, w, h, r):
        m = Image.new("L", (n, n), 0)
        ImageDraw.Draw(m).rounded_rectangle([u(x0), u(y0), u(x0 + w) - 1, u(y0 + h) - 1], radius=r * k, fill=255)
        return m
    tile = rrect_mask(0, 0, 256, 256, CORNER)
    img.paste(gradient(n, TILE_TOP, TILE_BOT, True), (0, 0), tile)
    # Rim: the tile mask minus the same shape inset by RIM_W.
    inset = rrect_mask(RIM_W, RIM_W, 256 - 2 * RIM_W, 256 - 2 * RIM_W, CORNER - RIM_W)
    rim = Image.new("L", (n, n), 0); rim.paste(tile, (0, 0)); rim.paste(0, (0, 0), inset)
    img.paste(Image.new("RGBA", (n, n), hex_rgb(RIM) + (255,)), (0, 0), rim)
    screen = rrect_mask(X, Y, W, H, CLIP_R)
    img.paste(Image.new("RGBA", (n, n), hex_rgb(WHITE) + (255,)), (0, 0), screen)
    layer = Image.new("RGBA", (n, n), (0, 0, 0, 0)); d = ImageDraw.Draw(layer)
    for p in design():
        if p[0] == "rect":
            d.rectangle([u(p[1]), u(p[2]), u(p[1] + p[3]) - 1, u(p[2] + p[4]) - 1], fill=p[5])
        elif p[0] == "bar":
            d.rounded_rectangle([u(p[1]), u(p[2]), u(p[1] + p[3]) - 1, u(p[2] + p[4]) - 1], radius=p[4] / 2 * k, fill=p[5])
        else:
            c = hex_rgb(p[4]) + (int(255 * p[5]),)
            d.ellipse([u(p[1] - p[3]), u(p[2] - p[3]), u(p[1] + p[3]), u(p[2] + p[3])], fill=c)
    clipped = Image.new("RGBA", (n, n), (0, 0, 0, 0)); clipped.paste(layer, (0, 0), screen)
    img.alpha_composite(clipped)
    # Camera bubble: gradient ring, tile-coloured face, white person.
    ring = Image.new("L", (n, n), 0)
    ImageDraw.Draw(ring).ellipse([u(BX - 41.5), u(BY - 41.5), u(BX + 41.5), u(BY + 41.5)], fill=255)
    img.paste(gradient(n, RING_A, RING_B, False, (u(BX - 41.5), u(BY - 41.5), u(BX + 41.5), u(BY + 41.5))), (0, 0), ring)
    d = ImageDraw.Draw(img)
    d.ellipse([u(BX - 37), u(BY - 37), u(BX + 37), u(BY + 37)], fill=TILE_BOT)
    s = 0.8
    d.ellipse([u(BX - 13 * s), u(BY - 14 * s + 2 - 13 * s), u(BX + 13 * s), u(BY - 14 * s + 2 + 13 * s)], fill="#FFFFFF")
    d.pieslice([u(BX - 24 * s), u(BY + 30 * s + 2 - 22 * s), u(BX + 24 * s), u(BY + 30 * s + 2 + 22 * s)], 180, 360, fill="#FFFFFF")
    return img.resize((size, size), Image.LANCZOS)


mac = lambda s: render(s, 100 / 1024)
win = lambda s: render(s, 0)
if __name__ == "__main__":
    open("rbs-icon.svg", "w").write(svg())
    mac(1024).save("icon-mac-1024.png"); mac(256).save("icon-mac-256.png")
    big = win(256); big.save("icon-win-256.png")
    big.save("rbs.ico", sizes=[(s, s) for s in (16, 24, 32, 48, 64, 128, 256)])
    tmp = tempfile.mkdtemp(); iconset = os.path.join(tmp, "rbs.iconset"); os.mkdir(iconset)
    for base in (16, 32, 128, 256, 512):
        mac(base).save(f"{iconset}/icon_{base}x{base}.png")
        mac(base * 2).save(f"{iconset}/icon_{base}x{base}@2x.png")
    subprocess.run(["iconutil", "--convert", "icns", "--output", "rbs.icns", iconset], check=True)
    shutil.rmtree(tmp)
