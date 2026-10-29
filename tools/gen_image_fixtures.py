#!/usr/bin/env python3
"""Generate tests/fixtures/images: PNG and JPEG files plus the RGBA pixels a
correct decoder must produce.

PNGs are written by the small encoder below (independent of PIL) so every
colour type, bit depth, filter type and interlace mode is covered; the
expected pixels are computed from the source samples, then cross-checked
against PIL where PIL has the same semantics.  JPEG references are PIL's
(libjpeg-turbo) own decode.

Manifest line: name|kind|width|height|max_diff|mean_diff
"""
import io, os, random, struct, sys, zlib
from PIL import Image

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tests", "fixtures", "images")
os.makedirs(OUT, exist_ok=True)
manifest = []
rnd = random.Random(1234)


def chunk(t, d):
    c = struct.pack(">I", len(d)) + t + d
    return c + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)


def paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    return b if pb <= pc else c


def filter_row(ft, row, prev, bpp):
    out = bytearray(len(row))
    for i, x in enumerate(row):
        a = row[i - bpp] if i >= bpp else 0
        b = prev[i]
        c = prev[i - bpp] if i >= bpp else 0
        pred = (0, a, b, (a + b) // 2, paeth(a, b, c))[ft]
        out[i] = (x - pred) & 255
    return bytes(out)


def pack_row(samples, depth):
    """samples: list of ints, packed MSB first for depth<8."""
    if depth == 8:
        return bytes(samples)
    if depth == 16:
        return b"".join(struct.pack(">H", s) for s in samples)
    out = bytearray()
    per = 8 // depth
    for i in range(0, len(samples), per):
        v = 0
        for j in range(per):
            s = samples[i + j] if i + j < len(samples) else 0
            v = (v << depth) | s
        out.append(v)
    return bytes(out)


ADAM7 = [(0, 0, 8, 8), (4, 0, 8, 8), (0, 4, 4, 8), (2, 0, 4, 4), (0, 2, 2, 4), (1, 0, 2, 2), (0, 1, 1, 2)]


def encode_png(w, h, ctype, depth, pix, interlace=0, palette=None, trns=None, filters="cycle", split_idat=False):
    """pix[y][x] is a tuple of samples (channels per colour type), or an index tuple (1,) for palette."""
    nch = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}[ctype]
    bpp = max(1, nch * depth // 8)
    raw = bytearray()
    passes = ADAM7 if interlace else [(0, 0, 1, 1)]
    rowno = 0
    for (x0, y0, dx, dy) in passes:
        ys = list(range(y0, h, dy))
        xs = list(range(x0, w, dx))
        if not ys or not xs:
            continue
        prev = bytes(len(pack_row([0] * (len(xs) * nch), depth)))
        for y in ys:
            samples = []
            for x in xs:
                samples.extend(pix[y][x])
            row = pack_row(samples, depth)
            ft = rowno % 5 if filters == "cycle" else int(filters)
            raw.append(ft)
            raw += filter_row(ft, row, prev, bpp)
            prev = row
            rowno += 1
    ihdr = struct.pack(">IIBBBBB", w, h, depth, ctype, 0, 0, interlace)
    out = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr)
    if palette is not None:
        out += chunk(b"PLTE", b"".join(bytes(p[:3]) for p in palette))
        alphas = bytes(p[3] for p in palette)
        if any(a != 255 for a in alphas):
            out += chunk(b"tRNS", alphas)
    if trns is not None:
        out += chunk(b"tRNS", trns)
    z = zlib.compress(bytes(raw), 6)
    if split_idat:
        n = max(1, len(z) // 3)
        for i in range(0, len(z), n):
            out += chunk(b"IDAT", z[i:i + n])
    else:
        out += chunk(b"IDAT", z)
    out += chunk(b"tEXt", b"Comment\x00unknown chunks are skipped")
    return out + chunk(b"IEND", b"")


def rgba_expected(w, h, ctype, depth, pix, palette=None, trns_key=None):
    """Straight RGBA8; 16-bit samples keep the high byte; tRNS keys compare full-depth samples."""
    to8 = (lambda v: v >> 8) if depth == 16 else (lambda v: v * 255 // ((1 << depth) - 1))
    out = bytearray()
    for y in range(h):
        for x in range(w):
            s = pix[y][x]
            if ctype == 0:
                g = to8(s[0])
                a = 0 if trns_key is not None and s[0] == trns_key[0] else 255
                out += bytes((g, g, g, a))
            elif ctype == 2:
                a = 0 if trns_key is not None and tuple(s) == tuple(trns_key) else 255
                out += bytes((to8(s[0]), to8(s[1]), to8(s[2]), a))
            elif ctype == 3:
                p = palette[s[0]]
                out += bytes(p[:4])
            elif ctype == 4:
                g = to8(s[0])
                out += bytes((g, g, g, to8(s[1])))
            else:
                out += bytes((to8(s[0]), to8(s[1]), to8(s[2]), to8(s[3])))
    return bytes(out)


def rand_pix(w, h, nch, depth):
    mx = (1 << depth) - 1
    return [[tuple(rnd.randint(0, mx) for _ in range(nch)) for _ in range(w)] for _ in range(h)]


def smooth_pix(w, h, nch, depth):
    """Gradients so the filters actually predict something."""
    mx = (1 << depth) - 1
    return [[tuple(((x * 7 + y * 5 + c * 40) * mx // (w * 7 + h * 5 + 160 + 1)) % (mx + 1) for c in range(nch)) for x in range(w)] for y in range(h)]


def add(name, kind, w, h, data, ext, rgba, maxd=0, meand=0.0):
    open(os.path.join(OUT, f"{name}.{ext}"), "wb").write(data)
    open(os.path.join(OUT, f"{name}.rgba"), "wb").write(rgba)
    manifest.append(f"{name}|{kind}|{w}|{h}|{maxd}|{meand}")


def pil_rgba(data):
    return Image.open(io.BytesIO(data)).convert("RGBA").tobytes()


# ---------------------------------------------------------------- PNG
sizes = [(1, 1), (13, 7), (8, 8), (31, 17), (3, 2)]
case = 0
for ctype, depths in [(0, [1, 2, 4, 8, 16]), (2, [8, 16]), (3, [1, 2, 4, 8]), (4, [8, 16]), (6, [8, 16])]:
    for depth in depths:
        for interlace in (0, 1):
            w, h = sizes[case % len(sizes)]
            case += 1
            if interlace and (w, h) == (1, 1):
                w, h = 13, 7
            nch = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}[ctype]
            palette = None
            trns = None
            trns_key = None
            if ctype == 3:
                n = min(1 << depth, 256)
                palette = [(rnd.randint(0, 255), rnd.randint(0, 255), rnd.randint(0, 255), rnd.choice([255, 255, 128, 0, 17])) for _ in range(n)]
                pix = [[(rnd.randrange(n),) for _ in range(w)] for _ in range(h)]
            else:
                pix = smooth_pix(w, h, nch, depth) if case % 2 else rand_pix(w, h, nch, depth)
            if ctype == 0 and case % 3 == 0:
                key = pix[0][0][0]
                trns = struct.pack(">H", key)
                trns_key = (key,)
            if ctype == 2 and case % 3 == 0:
                key = pix[0][0]
                trns = b"".join(struct.pack(">H", v) for v in key)
                trns_key = key
            name = f"png_c{ctype}_d{depth}_i{interlace}"
            data = encode_png(w, h, ctype, depth, pix, interlace, palette, trns, "cycle", split_idat=(case % 4 == 0))
            exp = rgba_expected(w, h, ctype, depth, pix, palette, trns_key)
            if depth != 16:
                ref = pil_rgba(data)
                if ctype == 0 and depth < 8 and trns_key is not None:
                    # PIL compares the tRNS key with its already-scaled 8-bit samples (a PIL
                    # quirk); the PNG spec says full-depth sample == key, which `exp` follows.
                    # Check colours only.
                    assert all(ref[i:i + 3] == exp[i:i + 3] for i in range(0, len(ref), 4)), name
                else:
                    assert ref == exp, f"{name}: my reference disagrees with PIL"
            add(name, "exact", w, h, data, "png", exp)

# every filter type on its own, 24-bit, bigger image
for ft in range(5):
    w, h = 40, 30
    pix = smooth_pix(w, h, 3, 8)
    data = encode_png(w, h, 2, 8, pix, 0, None, None, str(ft))
    exp = rgba_expected(w, h, 2, 8, pix)
    assert pil_rgba(data) == exp
    add(f"png_filter{ft}", "exact", w, h, data, "png", exp)

# ---------------------------------------------------------------- JPEG
def source(w, h, mode="RGB"):
    im = Image.new("RGB", (w, h))
    px = im.load()
    for y in range(h):
        for x in range(w):
            r = int(255 * x / max(1, w - 1))
            g = int(255 * y / max(1, h - 1))
            b = (x * 13 + y * 7) % 256
            if (x // 6 + y // 6) % 2 == 0:
                r, g, b = 255 - r, 255 - g, 255 - b
            if x == w // 2:
                r = g = b = 0
            px[x, y] = (r, g, b)
    # a little noise
    for _ in range(w * h // 8):
        x, y = rnd.randrange(w), rnd.randrange(h)
        px[x, y] = tuple(rnd.randrange(256) for _ in range(3))
    return im.convert(mode)


def jpeg_case(name, w, h, mode="RGB", maxd=3, meand=0.6, **kw):
    im = source(w, h, mode)
    buf = io.BytesIO()
    try:
        im.save(buf, "JPEG", **kw)
    except TypeError as e:
        print("skip", name, e, file=sys.stderr)
        return
    data = buf.getvalue()
    ref = Image.open(io.BytesIO(data)).convert("RGBA")
    add(name, "jpeg", w, h, data, "jpg", ref.tobytes(), maxd, meand)


for sub, sname in [(0, "444"), (1, "422"), (2, "420")]:
    for q in (30, 75, 95):
        jpeg_case(f"jpg_{sname}_q{q}", 61, 45, quality=q, subsampling=sub)
    jpeg_case(f"jpg_{sname}_prog", 61, 45, quality=80, subsampling=sub, progressive=True)
    jpeg_case(f"jpg_{sname}_opt", 33, 17, quality=80, subsampling=sub, optimize=True)
    jpeg_case(f"jpg_{sname}_prog_opt", 70, 50, quality=60, subsampling=sub, progressive=True, optimize=True)
    jpeg_case(f"jpg_{sname}_tiny", 5, 3, quality=90, subsampling=sub)
    jpeg_case(f"jpg_{sname}_1px", 1, 1, quality=90, subsampling=sub)
    jpeg_case(f"jpg_{sname}_3x5", 3, 5, quality=90, subsampling=sub)
    jpeg_case(f"jpg_{sname}_4x4", 4, 4, quality=90, subsampling=sub)
    jpeg_case(f"jpg_{sname}_wide", 130, 9, quality=85, subsampling=sub)
    jpeg_case(f"jpg_{sname}_16", 16, 16, quality=90, subsampling=sub)
jpeg_case("jpg_gray", 37, 29, mode="L", quality=85)
jpeg_case("jpg_gray_prog", 37, 29, mode="L", quality=85, progressive=True)
jpeg_case("jpg_cmyk", 24, 18, mode="CMYK", maxd=4, meand=1.0, quality=90)
for kw, nm in [({"restart_marker_blocks": 3}, "rst_blocks"), ({"restart_marker_rows": 1}, "rst_rows")]:
    jpeg_case(f"jpg_420_{nm}", 61, 45, quality=80, subsampling=2, **kw)
    jpeg_case(f"jpg_444_{nm}", 61, 45, quality=80, subsampling=0, **kw)

open(os.path.join(OUT, "manifest.txt"), "w").write("\n".join(manifest) + "\n")
print(len(manifest), "fixtures,", sum(os.path.getsize(os.path.join(OUT, f)) for f in os.listdir(OUT)) // 1024, "KiB")
