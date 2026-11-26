#!/usr/bin/env python3
"""Regenerates the application icon and brand mark files from one design description (see README.md).

The design is a 6-blade hexagonal camera-aperture mark in a 256-unit box (and 64-unit
standalone mark); the SVG masters and every raster are drawn from the same geometry, so
they cannot drift apart. Needs Pillow; `rbs.icns` also needs macOS `iconutil`. Run from
this directory.
"""
import math, os, shutil, subprocess, tempfile
from PIL import Image, ImageChops, ImageDraw, ImageFilter

SS = 4  # supersampling for the rasters

# --- Design (256-unit box) -------------------------------------------------------
TILE_TOP, TILE_BOT = "#1D1E24", "#0D0E12"
RIM_TOP, RIM_BOT = "#404352", "#22242C"
RIM_W = 2.5
CORNER = 57

# Anodized RBS violet (lit softly from top-left, matching color.brand #8a63ff / #5326d9).
BRAND_LIGHT = "#A07DFF"
BRAND_MID = "#7B50F6"
BRAND_DARK = "#4E23D1"
SPECULAR = "#D8C8FF"
SHADE = "#280D78"
MARK_DARK = "#8A63FF"

CX, CY = 128.0, 128.0
R_OUT, R_IN = 90.0, 37.0
GAP = 5.2
BLADE_R = 4.6


def hex_rgb(h):
    return tuple(int(h[i:i + 2], 16) for i in (1, 3, 5))


def intersect(p1, d1, p2, d2):
    """Intersection of two lines (p1 + t*d1) and (p2 + u*d2)."""
    det = d1[0] * d2[1] - d1[1] * d2[0]
    dx, dy = p2[0] - p1[0], p2[1] - p1[1]
    t = (dx * d2[1] - dy * d2[0]) / det
    return (p1[0] + t * d1[0], p1[1] + t * d1[1])


def blade_quad(k, cx, cy, r_out, r_in, gap):
    """Exact 4 vertices of blade k (0..5) with uniform parallel seams of width `gap`."""
    a0 = math.radians(-90.0 + k * 60.0)
    a1 = math.radians(-90.0 + (k + 1) * 60.0)
    d0 = (math.cos(a0), math.sin(a0))
    d1 = (math.cos(a1), math.sin(a1))
    # Inward normals perpendicular to each ray (pointing into blade k).
    n0 = (-d0[1], d0[0])
    n1 = (d1[1], -d1[0])
    p0 = (cx + n0[0] * gap / 2, cy + n0[1] * gap / 2)
    p1 = (cx + n1[0] * gap / 2, cy + n1[1] * gap / 2)

    v_out0 = (cx + r_out * d0[0], cy + r_out * d0[1])
    v_out1 = (cx + r_out * d1[0], cy + r_out * d1[1])
    v_in0 = (cx + r_in * d0[0], cy + r_in * d0[1])
    v_in1 = (cx + r_in * d1[0], cy + r_in * d1[1])
    edge_dir = (v_out1[0] - v_out0[0], v_out1[1] - v_out0[1])

    q0 = intersect(p0, d0, v_out0, edge_dir)
    q1 = intersect(p1, d1, v_out0, edge_dir)
    q2 = intersect(p1, d1, v_in0, edge_dir)
    q3 = intersect(p0, d0, v_in0, edge_dir)
    return [q0, q1, q2, q3]


def rounded_poly_points(pts, radius, steps=8):
    """Subdivides a convex polygon with circular-like quadratic corner fillets."""
    out = []
    n = len(pts)
    for i in range(n):
        prev_p = pts[(i - 1) % n]
        curr_p = pts[i]
        next_p = pts[(i + 1) % n]
        v1 = (prev_p[0] - curr_p[0], prev_p[1] - curr_p[1])
        v2 = (next_p[0] - curr_p[0], next_p[1] - curr_p[1])
        l1 = math.hypot(*v1)
        l2 = math.hypot(*v2)
        r = min(radius, l1 * 0.4, l2 * 0.4)
        a = (curr_p[0] + v1[0] * r / l1, curr_p[1] + v1[1] * r / l1)
        b = (curr_p[0] + v2[0] * r / l2, curr_p[1] + v2[1] * r / l2)
        for s in range(steps + 1):
            t = s / steps
            u = 1 - t
            x = u * u * a[0] + 2 * u * t * curr_p[0] + t * t * b[0]
            y = u * u * a[1] + 2 * u * t * curr_p[1] + t * t * b[1]
            out.append((x, y))
    return out


def rounded_poly_svg(pts, radius):
    """SVG path data (`M ... L ... Q ... Z`) for a rounded polygon."""
    segs = []
    n = len(pts)
    for i in range(n):
        prev_p = pts[(i - 1) % n]
        curr_p = pts[i]
        next_p = pts[(i + 1) % n]
        v1 = (prev_p[0] - curr_p[0], prev_p[1] - curr_p[1])
        v2 = (next_p[0] - curr_p[0], next_p[1] - curr_p[1])
        l1 = math.hypot(*v1)
        l2 = math.hypot(*v2)
        r = min(radius, l1 * 0.4, l2 * 0.4)
        a = (curr_p[0] + v1[0] * r / l1, curr_p[1] + v1[1] * r / l1)
        b = (curr_p[0] + v2[0] * r / l2, curr_p[1] + v2[1] * r / l2)
        cmd = "M" if i == 0 else "L"
        segs.append(f"{cmd}{a[0]:.2f} {a[1]:.2f} Q{curr_p[0]:.2f} {curr_p[1]:.2f} {b[0]:.2f} {b[1]:.2f}")
    return " ".join(segs) + " Z"


def mark_path_64():
    """Combined 6-blade SVG path in a 64 x 64 viewBox (shared by rbs-mark.svg and BrandMark)."""
    parts = []
    for k in range(6):
        q = blade_quad(k, 32.0, 32.0, 28.5, 11.5, 2.6)
        parts.append(rounded_poly_svg(q, 1.5))
    return " ".join(parts)


# --- SVG -------------------------------------------------------------------------
def mark_svg():
    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <!-- The RBS mark: a 6-blade hexagonal aperture. Dark-theme colour; in the app BrandMark draws it from theme tokens. -->
  <path d="{mark_path_64()}" fill="{MARK_DARK}"/>
</svg>
'''


def icon_svg():
    blades = []
    for k in range(6):
        d = rounded_poly_svg(blade_quad(k, CX, CY, R_OUT, R_IN, GAP), BLADE_R)
        blades.append(
            f'    <path d="{d}" fill="url(#blade)" stroke="url(#bevel)" stroke-width="1.4"/>'
        )
    blade_xml = "\n".join(blades)
    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256">
  <!-- The application icon: anodized RBS violet hexagonal aperture on a dark obsidian tile, lit softly from top-left. Fixed colours, the OS does not follow the theme. -->
  <defs>
    <linearGradient id="tile" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="{TILE_TOP}"/>
      <stop offset="1" stop-color="{TILE_BOT}"/>
    </linearGradient>
    <linearGradient id="rim" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="{RIM_TOP}"/>
      <stop offset="1" stop-color="{RIM_BOT}"/>
    </linearGradient>
    <linearGradient id="blade" x1="48" y1="36" x2="208" y2="220" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="{BRAND_LIGHT}"/>
      <stop offset="0.52" stop-color="{BRAND_MID}"/>
      <stop offset="1" stop-color="{BRAND_DARK}"/>
    </linearGradient>
    <linearGradient id="bevel" x1="48" y1="36" x2="208" y2="220" gradientUnits="userSpaceOnUse">
      <stop offset="0" stop-color="{SPECULAR}" stop-opacity="0.55"/>
      <stop offset="0.45" stop-color="{SPECULAR}" stop-opacity="0.06"/>
      <stop offset="1" stop-color="{SHADE}" stop-opacity="0.5"/>
    </linearGradient>
    <filter id="drop" x="-20%" y="-20%" width="140%" height="140%">
      <feDropShadow dx="2" dy="6" stdDeviation="6" flood-color="#000000" flood-opacity="0.55"/>
    </filter>
  </defs>
  <rect width="256" height="256" rx="{CORNER}" fill="url(#tile)"/>
  <rect x="{RIM_W / 2}" y="{RIM_W / 2}" width="{256 - RIM_W}" height="{256 - RIM_W}" rx="{CORNER - RIM_W / 2}" fill="none" stroke="url(#rim)" stroke-width="{RIM_W}"/>
  <g filter="url(#drop)">
{blade_xml}
  </g>
</svg>
'''


# --- Raster ----------------------------------------------------------------------
def gradient3(n, c0, c1, c2, p0, p1):
    """Linear 3-stop gradient from p0 to p1 across an n x n RGBA image."""
    m = min(n, 256)
    s = m / n
    q0 = (p0[0] * s, p0[1] * s)
    q1 = (p1[0] * s, p1[1] * s)
    g = Image.new("RGBA", (m, m))
    px = g.load()
    a, b, c = hex_rgb(c0), hex_rgb(c1), hex_rgb(c2)
    vx, vy = q1[0] - q0[0], q1[1] - q0[1]
    denom = max(vx * vx + vy * vy, 1.0)
    for y in range(m):
        dy = y - q0[1]
        for x in range(m):
            t = ((x - q0[0]) * vx + dy * vy) / denom
            if t <= 0.0:
                col = a
            elif t >= 1.0:
                col = c
            elif t < 0.52:
                u = t / 0.52
                col = tuple(int(a[i] + (b[i] - a[i]) * u) for i in range(3))
            else:
                u = (t - 0.52) / 0.48
                col = tuple(int(b[i] + (c[i] - b[i]) * u) for i in range(3))
            px[x, y] = col + (255,)
    return g if m == n else g.resize((n, n), Image.BILINEAR)


def gradient2(n, c0, c1, p0, p1):
    return gradient3(
        n,
        c0,
        "#{:02x}{:02x}{:02x}".format(*(int((hex_rgb(c0)[i] + hex_rgb(c1)[i]) / 2) for i in range(3))),
        c1,
        p0,
        p1,
    )


def render(size, margin_share, gap_override=None):
    """`size` px canvas; the 256-unit design fills it minus `margin_share` on each side."""
    n = size * SS
    inner = n * (1 - 2 * margin_share)
    k = inner / 256.0
    off = n * margin_share
    u = lambda v: v * k + off
    img = Image.new("RGBA", (n, n), (0, 0, 0, 0))

    def rrect_mask(x0, y0, w, h, r):
        m = Image.new("L", (n, n), 0)
        ImageDraw.Draw(m).rounded_rectangle(
            [u(x0), u(y0), u(x0 + w) - 1, u(y0 + h) - 1], radius=r * k, fill=255
        )
        return m

    # Dark obsidian squircle tile + subtle top-left lit rim.
    tile = rrect_mask(0, 0, 256, 256, CORNER)
    img.paste(gradient2(n, TILE_TOP, TILE_BOT, (u(0), u(0)), (u(0), u(256))), (0, 0), tile)
    inset = rrect_mask(RIM_W, RIM_W, 256 - 2 * RIM_W, 256 - 2 * RIM_W, CORNER - RIM_W)
    rim = ImageChops.subtract(tile, inset)
    img.paste(gradient2(n, RIM_TOP, RIM_BOT, (u(0), u(0)), (u(256), u(256))), (0, 0), rim)

    gap = gap_override if gap_override is not None else GAP
    blade_polys = [
        [(u(x), u(y)) for x, y in rounded_poly_points(blade_quad(i, CX, CY, R_OUT, R_IN, gap), BLADE_R)]
        for i in range(6)
    ]

    # Combined blade mask.
    blades_mask = Image.new("L", (n, n), 0)
    md = ImageDraw.Draw(blades_mask)
    for poly in blade_polys:
        md.polygon(poly, fill=255)

    # Soft Apple-style grounded contact shadow (offset bottom-right).
    shadow_alpha = Image.new("L", (n, n), 0)
    sd = ImageDraw.Draw(shadow_alpha)
    dx_s, dy_s = 2.0 * k, 6.0 * k
    for poly in blade_polys:
        sd.polygon([(x + dx_s, y + dy_s) for x, y in poly], fill=145)
    shadow_alpha = shadow_alpha.filter(ImageFilter.GaussianBlur(radius=max(1.0, 6.0 * k)))
    shadow_alpha = ImageChops.multiply(shadow_alpha, tile)
    shadow_layer = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    shadow_layer.putalpha(shadow_alpha)
    img.alpha_composite(shadow_layer)

    # Anodized RBS violet fill lit from top-left (48, 36) -> (208, 220).
    blade_fill = gradient3(n, BRAND_LIGHT, BRAND_MID, BRAND_DARK, (u(48), u(36)), (u(208), u(220)))
    blade_layer = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    blade_layer.paste(blade_fill, (0, 0), blades_mask)

    # Top-left single-edge specular highlight inside the blade boundary.
    shift = max(1, int(round(1.6 * k)))
    shifted_tl = ImageChops.offset(blades_mask, shift, shift)
    top_edge = ImageChops.subtract(blades_mask, shifted_tl)
    spec_grad = gradient2(n, SPECULAR, "#000000", (u(40), u(32)), (u(200), u(200))).convert("L")
    spec_alpha = ImageChops.multiply(top_edge, spec_grad)
    spec_alpha = spec_alpha.point(lambda v: int(v * 0.65))
    spec_layer = Image.new("RGBA", (n, n), hex_rgb(SPECULAR) + (0,))
    spec_layer.putalpha(spec_alpha)
    blade_layer.alpha_composite(spec_layer)

    # Subtle bottom-right edge shade inside the blade boundary.
    shifted_br = ImageChops.offset(blades_mask, -shift, -shift)
    bot_edge = ImageChops.subtract(blades_mask, shifted_br)
    shade_alpha = bot_edge.point(lambda v: int(v * 0.42))
    shade_layer = Image.new("RGBA", (n, n), hex_rgb(SHADE) + (0,))
    shade_layer.putalpha(shade_alpha)
    blade_layer.alpha_composite(shade_layer)

    img.alpha_composite(blade_layer)
    return img.resize((size, size), Image.LANCZOS)


mac = lambda s: render(s, 100 / 1024, gap_override=7.5 if s <= 32 else None)
win = lambda s: render(s, 0, gap_override=7.5 if s <= 32 else None)
if __name__ == "__main__":
    open("rbs-icon.svg", "w").write(icon_svg())
    open("rbs-mark.svg", "w").write(mark_svg())
    mac(1024).save("icon-mac-1024.png")
    mac(256).save("icon-mac-256.png")
    big = win(256)
    big.save("icon-win-256.png")
    win(32).save("favicon-32.png")
    win(16).save("favicon-16.png")
    # Multi-resolution Windows .ico with optical gap compensation at small sizes.
    ico_sizes = [16, 24, 32, 48, 64, 128, 256]
    ico_imgs = [win(s) for s in ico_sizes]
    ico_imgs[-1].save("rbs.ico", format="ICO", append_images=ico_imgs[:-1])
    tmp = tempfile.mkdtemp()
    iconset = os.path.join(tmp, "rbs.iconset")
    os.mkdir(iconset)
    for base in (16, 32, 128, 256, 512):
        mac(base).save(f"{iconset}/icon_{base}x{base}.png")
        mac(base * 2).save(f"{iconset}/icon_{base}x{base}@2x.png")
    subprocess.run(["iconutil", "--convert", "icns", "--output", "rbs.icns", iconset], check=True)
    shutil.rmtree(tmp)

