#!/usr/bin/env python3
"""Generate the Synka icon without third-party packages (no Pillow needed).

Design: two linked rounded frames for a desktop-to-cloud connection, in white and
ice blue on the existing dark blue square. Rendered with 4x4 supersampling,
encoded as PNG, and packed into a multi-size ICO (PNG entries).

The same ICO lives in two places, and both are written by default:
  resources/app.ico    the engine's icon (tray, exe), embedded by build.rs via resources/app.rc
  ui/Assets/app.ico    the window's icon (ApplicationIcon in ui/Synka.csproj)
The MSIX tiles, splash image and title-bar PNGs can be regenerated with --assets.

Usage (run with -I so a stray module in the current directory is never imported):
  python -I packaging/make_icons.py                  # writes both app.ico files
  python -I packaging/make_icons.py --assets         # writes app.ico and all MSIX PNGs
  python -I packaging/make_icons.py --ico PATH       # write the ICO to PATH only (repeatable)
  python -I packaging/make_icons.py --png 150 PATH   # one square PNG (e.g. MSIX logos)
  python -I packaging/make_icons.py --lock-icons DIR # LockedByMe.ico + LockedByOther.ico
                                                     # (Explorer lock state, ui/Assets)
"""

import argparse
import os
import struct
import zlib

BG = (0x0B, 0x3D, 0x91)
FG = (0xFF, 0xFF, 0xFF)
ACCENT = (0x6E, 0xDC, 0xF5)
ICO_SIZES = [16, 24, 32, 48, 64, 256]
SUPERSAMPLE = 4
CORNER_RADIUS = 0.22  # fraction of the icon edge

def in_rounded_square(x, y, r=CORNER_RADIUS):
    cx = min(max(x, r), 1.0 - r)
    cy = min(max(y, r), 1.0 - r)
    return (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def in_round_rect(x, y, x0, y0, x1, y1, r):
    cx = min(max(x, x0 + r), x1 - r)
    cy = min(max(y, y0 + r), y1 - r)
    return x0 <= x <= x1 and y0 <= y <= y1 and (x - cx) ** 2 + (y - cy) ** 2 <= r * r


def in_frame(x, y, x0, y0, x1, y1, r, stroke):
    return in_round_rect(x, y, x0, y0, x1, y1, r) and not in_round_rect(
        x, y, x0 + stroke, y0 + stroke, x1 - stroke, y1 - stroke, r - stroke
    )


def mark_color(x, y):
    # The two linked frames are deliberately broad enough to survive at 16 px.
    if in_frame(x, y, 0.40, 0.34, 0.81, 0.76, 0.14, 0.085):
        return ACCENT
    if in_frame(x, y, 0.19, 0.24, 0.60, 0.66, 0.14, 0.085):
        return FG
    return None


def render_rgba(size, mark_only=False):
    """Return raw RGBA bytes for a square app icon or its transparent title-bar mark."""
    out = bytearray(size * size * 4)
    n_samples = SUPERSAMPLE * SUPERSAMPLE
    step = 1.0 / SUPERSAMPLE
    for py in range(size):
        for px in range(size):
            inside = 0
            acc_r = acc_g = acc_b = 0
            for sy in range(SUPERSAMPLE):
                y = (py + (sy + 0.5) * step) / size
                for sx in range(SUPERSAMPLE):
                    x = (px + (sx + 0.5) * step) / size
                    mark = mark_color(x, y)
                    if mark_only:
                        if mark is None:
                            continue
                        c = mark
                    else:
                        if not in_rounded_square(x, y):
                            continue
                        c = mark or BG
                    inside += 1
                    acc_r += c[0]
                    acc_g += c[1]
                    acc_b += c[2]
            if inside:
                o = (py * size + px) * 4
                out[o] = acc_r // inside
                out[o + 1] = acc_g // inside
                out[o + 2] = acc_b // inside
                out[o + 3] = round(255 * inside / n_samples)
    return bytes(out)


# Padlock for the Explorer lock state: blue when locked by the signed-in user, red when
# locked by someone else. Shackle in a darker shade of the same colour.
LOCK_MINE = (0x0B, 0x3D, 0x91)
LOCK_OTHER = (0xC4, 0x2B, 0x1C)
LOCK_ICO_SIZES = [16, 20, 24, 32, 40, 48, 64]


def lock_color(x, y, body):
    """Colour of the padlock at unit coordinates, or None outside it."""
    if in_round_rect(x, y, 0.16, 0.44, 0.84, 0.92, 0.09):
        in_hole = (x - 0.5) ** 2 + (y - 0.63) ** 2 <= 0.07 ** 2 or (0.465 <= x <= 0.535 and 0.63 <= y <= 0.79)
        return FG if in_hole else body
    shackle = tuple(c * 3 // 5 for c in body)
    d2 = (x - 0.5) ** 2 + (y - 0.44) ** 2
    if y <= 0.44 and 0.17 ** 2 <= d2 <= 0.27 ** 2:
        return shackle
    return None


def render_lock_rgba(size, body):
    out = bytearray(size * size * 4)
    n_samples = SUPERSAMPLE * SUPERSAMPLE
    step = 1.0 / SUPERSAMPLE
    for py in range(size):
        for px in range(size):
            inside = 0
            acc = [0, 0, 0]
            for sy in range(SUPERSAMPLE):
                y = (py + (sy + 0.5) * step) / size
                for sx in range(SUPERSAMPLE):
                    x = (px + (sx + 0.5) * step) / size
                    c = lock_color(x, y, body)
                    if c is None:
                        continue
                    inside += 1
                    for i in range(3):
                        acc[i] += c[i]
            if inside:
                o = (py * size + px) * 4
                out[o:o + 3] = bytes(v // inside for v in acc)
                out[o + 3] = round(255 * inside / n_samples)
    return bytes(out)


def encode_png(size, rgba, height=None):
    if height is None:
        height = size
    raw = bytearray()
    row = size * 4
    for j in range(height):
        raw.append(0)  # filter type: none
        raw += rgba[j * row:(j + 1) * row]

    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", size, height, 8, 6, 0, 0, 0)  # 8-bit RGBA
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def encode_ico(entries):
    """entries: list of (size, png_bytes)."""
    header = struct.pack("<HHH", 0, 1, len(entries))
    directory = b""
    payload = b""
    offset = 6 + 16 * len(entries)
    for size, png in entries:
        dim = 0 if size >= 256 else size  # 0 means 256 in ICO directory entries
        directory += struct.pack("<BBBBHHII", dim, dim, 0, 0, 1, 32, len(png), offset)
        payload += png
        offset += len(png)
    return header + directory + payload


def centered_icon(width, height, icon_size):
    """Place the same app mark on a transparent Windows wide tile or splash canvas."""
    icon = render_rgba(icon_size)
    out = bytearray(width * height * 4)
    left = (width - icon_size) // 2
    top = (height - icon_size) // 2
    for y in range(icon_size):
        start = ((top + y) * width + left) * 4
        out[start:start + icon_size * 4] = icon[y * icon_size * 4:(y + 1) * icon_size * 4]
    return bytes(out)


def write_png(path, width, height, rgba):
    with open(path, "wb") as f:
        f.write(encode_png(width, rgba, height))
    print(f"wrote {path} ({width}x{height})")


def write_msix_assets(repo):
    assets = os.path.join(repo, "ui", "Assets")
    square_variants = {
        "Square150x150Logo": [("", 150), (".scale-125", 188), (".scale-150", 225),
                              (".scale-200", 300), (".scale-400", 600)],
        "Square44x44Logo": [("", 44), (".scale-125", 55), (".scale-150", 66),
                            (".scale-200", 88), (".scale-400", 176)]
                           + [(f".targetsize-{s}", s) for s in (16, 24, 32, 48, 256)],
        "StoreLogo": [("", 50), (".scale-125", 63), (".scale-150", 75),
                      (".scale-200", 100), (".scale-400", 200)],
    }
    for stem, variants in square_variants.items():
        for suffix, size in variants:
            write_png(os.path.join(assets, f"{stem}{suffix}.png"), size, size, render_rgba(size))

    for suffix, size in ((".scale-100", 16), (".scale-125", 20), (".scale-150", 24),
                         (".scale-200", 32), (".scale-400", 64)):
        write_png(os.path.join(assets, f"TitleBarIcon{suffix}.png"), size, size,
                  render_rgba(size, mark_only=True))

    for stem, variants, fraction in (
        ("Wide310x150Logo", [("", 310, 150), (".scale-125", 388, 188),
                              (".scale-150", 465, 225), (".scale-200", 620, 300),
                              (".scale-400", 1240, 600)], 0.91),
        ("SplashScreen", [("", 620, 300), (".scale-125", 775, 375),
                           (".scale-150", 930, 450), (".scale-200", 1240, 600),
                           (".scale-400", 2480, 1200)], 0.60),
    ):
        for suffix, width, height in variants:
            icon_size = round(height * fraction)
            write_png(os.path.join(assets, f"{stem}{suffix}.png"), width, height,
                      centered_icon(width, height, icon_size))


def main():
    repo = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    default_icos = [
        os.path.join(repo, "resources", "app.ico"),  # engine (build.rs, resources/app.rc)
        os.path.join(repo, "ui", "Assets", "app.ico"),  # window (ui/Synka.csproj)
    ]
    parser = argparse.ArgumentParser(description="Generate Synka icons.")
    parser.add_argument("--ico", action="append",
                        help="output ICO path, repeatable "
                             "(default: resources/app.ico and ui/Assets/app.ico)")
    parser.add_argument("--png", nargs=2, metavar=("SIZE", "PATH"),
                        help="write a single square PNG instead of the ICO")
    parser.add_argument("--lock-icons", metavar="DIR",
                        help="write LockedByMe.ico and LockedByOther.ico into DIR (ui/Assets)")
    parser.add_argument("--assets", action="store_true",
                        help="write all MSIX tile, splash and title-bar PNGs and both app.ico files")
    args = parser.parse_args()

    if args.lock_icons:
        os.makedirs(args.lock_icons, exist_ok=True)
        for name, body in (("LockedByMe.ico", LOCK_MINE), ("LockedByOther.ico", LOCK_OTHER)):
            entries = [(s, encode_png(s, render_lock_rgba(s, body))) for s in LOCK_ICO_SIZES]
            path = os.path.join(args.lock_icons, name)
            with open(path, "wb") as f:
                f.write(encode_ico(entries))
            print(f"wrote {path} sizes={LOCK_ICO_SIZES}")
        return

    if args.png:
        size, path = int(args.png[0]), args.png[1]
        data = encode_png(size, render_rgba(size))
        with open(path, "wb") as f:
            f.write(data)
        print(f"wrote {path} ({size}x{size})")
        return

    if args.assets:
        write_msix_assets(repo)

    entries = [(s, encode_png(s, render_rgba(s))) for s in ICO_SIZES]
    ico = encode_ico(entries)
    for path in args.ico or default_icos:
        os.makedirs(os.path.dirname(os.path.abspath(path)), exist_ok=True)
        with open(path, "wb") as f:
            f.write(ico)
        print(f"wrote {path} sizes={ICO_SIZES}")


if __name__ == "__main__":
    main()
