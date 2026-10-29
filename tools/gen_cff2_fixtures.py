#!/usr/bin/env python3
"""Variable CFF2 test fonts and reference outlines, made with fontTools.

Writes into tests/fixtures/fonts/:
  vf_light.otf    wght 100..900, default 100 (like Noto Sans CJK VF), two masters
  vf_regular.otf  wght 100..900, default 400, three masters, an avar bend, hints
  cff2_ref.txt    for each font, weight and glyph: advance and the outline drawn by
                  fontTools at that location (an independent implementation)

Format of cff2_ref.txt: lines `font <file>`, `at <weight>`, `glyph <codepoint> <advance>` then
M/L/C/Z lines with coordinates.
"""
import os, sys, tempfile
from fontTools.fontBuilder import FontBuilder
from fontTools.pens.t2CharStringPen import T2CharStringPen
from fontTools.pens.recordingPen import RecordingPen
from fontTools.designspaceLib import DesignSpaceDocument, AxisDescriptor, SourceDescriptor
from fontTools.ttLib import TTFont
from fontTools import varLib

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tests", "fixtures", "fonts")
K = 0.5522847498

def circle(pen, cx, cy, r, ccw=False):
    pts = [(cx + r, cy), (cx, cy + r), (cx - r, cy), (cx, cy - r)] if ccw else [(cx + r, cy), (cx, cy - r), (cx - r, cy), (cx, cy + r)]
    pen.moveTo(pts[0])
    d = K * r
    if ccw:
        pen.curveTo((cx + r, cy + d), (cx + d, cy + r), (cx, cy + r))
        pen.curveTo((cx - d, cy + r), (cx - r, cy + d), (cx - r, cy))
        pen.curveTo((cx - r, cy - d), (cx - d, cy - r), (cx, cy - r))
        pen.curveTo((cx + d, cy - r), (cx + r, cy - d), (cx + r, cy))
    else:
        pen.curveTo((cx + r, cy - d), (cx + d, cy - r), (cx, cy - r))
        pen.curveTo((cx - d, cy - r), (cx - r, cy - d), (cx - r, cy))
        pen.curveTo((cx - r, cy + d), (cx - d, cy + r), (cx, cy + r))
        pen.curveTo((cx + d, cy + r), (cx + r, cy + d), (cx + r, cy))
    pen.closePath()

def poly(pen, pts):
    pen.moveTo(pts[0])
    for p in pts[1:]:
        pen.lineTo(p)
    pen.closePath()

# every glyph is drawn from the stem weight t in 0..1 with identical structure
GLYPHS = {}
def glyph(name, cp=None):
    def deco(f):
        GLYPHS[name] = (cp, f)
        return f
    return deco

@glyph(".notdef")
def _(pen, t):
    poly(pen, [(50, 0), (450, 0), (450, 700), (50, 700)])
    poly(pen, [(100 + 40 * t, 50 + 40 * t), (100 + 40 * t, 650 - 40 * t), (400 - 40 * t, 650 - 40 * t), (400 - 40 * t, 50 + 40 * t)])
    return 500 + 20 * t

@glyph("space", 0x20)
def _(pen, t):
    return 250 + 50 * t

@glyph("I", 0x49)
def _(pen, t):
    s = 60 + 200 * t
    poly(pen, [(100, 0), (100 + s, 0), (100 + s, 700), (100, 700)])
    return 200 + s

@glyph("H", 0x48)   # lines only; gets stem hints and a hintmask
def _(pen, t):
    s = 60 + 140 * t
    poly(pen, [(80, 0), (80 + s, 0), (80 + s, 300 - s / 2), (460 - s, 300 - s / 2), (460 - s, 0), (460, 0), (460, 700), (460 - s, 700), (460 - s, 300 + s / 2), (80 + s, 300 + s / 2), (80 + s, 700), (80, 700)])
    return 540 + 60 * t

@glyph("O", 0x4F)
def _(pen, t):
    s = 50 + 150 * t
    circle(pen, 350, 350, 330)
    circle(pen, 350, 350, 330 - s, ccw=True)
    return 700 + 40 * t

@glyph("A", 0x41)   # slanted lines
def _(pen, t):
    s = 80 + 120 * t
    poly(pen, [(20, 0), (20 + s, 0), (330, 520 - 60 * t), (640 - s - 20, 0), (640 - 20, 0), (330 + s / 2, 700), (330 - s / 2, 700)])
    poly(pen, [(200 + 30 * t, 180), (460 - 30 * t, 180), (460 - 30 * t, 180 + s / 2), (200 + 30 * t, 180 + s / 2)])
    return 660 + 20 * t

@glyph("g", 0x67)   # curves and lines mixed, arcs with horizontal/vertical tangents
def _(pen, t):
    s = 40 + 100 * t
    pen.moveTo((60, 300))
    pen.curveTo((60, 500), (160, 560), (300, 560))
    pen.lineTo((420, 560))
    pen.lineTo((420, 560 - s))
    pen.lineTo((330 - t * 10, 560 - s))
    pen.curveTo((200, 560 - s), (60 + s, 500 - s / 2), (60 + s, 300))
    pen.curveTo((60 + s, 120), (150, 40 + s), (300, 40 + s))
    pen.curveTo((380, 40 + s), (420 - s, 110), (420 - s, 200 - s))
    pen.lineTo((420, 200 - s))
    pen.curveTo((420, 40), (360, -150 - 30 * t), (220, -150 - 30 * t))
    pen.curveTo((120, -150 - 30 * t), (60, -80), (60, 300 - 180))
    pen.closePath()
    return 520 + 30 * t

@glyph("uni3042", 0x3042)   # a kana-like glyph: many curves, one wide counter
def _(pen, t):
    s = 30 + 80 * t
    pen.moveTo((120, 640))
    pen.curveTo((300, 700), (700, 700), (880, 620 - s))
    pen.lineTo((880, 620))
    pen.curveTo((700, 720), (300, 730), (120, 640 + s))
    pen.closePath()
    pen.moveTo((500, 840))
    pen.curveTo((560 + s, 700), (560 + s, 300), (500, -40))
    pen.lineTo((500 - s, -40))
    pen.curveTo((470, 300 + t * 20), (470, 700 + t * 20), (500 - s, 840))
    pen.closePath()
    circle(pen, 520, 280, 170 + 40 * t)
    circle(pen, 520, 280, 90, ccw=True)
    return 1000

@glyph("uni900F", 0x900F)   # CJK-like: lots of straight and flexed strokes
def _(pen, t):
    s = 40 + 70 * t
    for i in range(6):
        y = 80 + i * 140
        poly(pen, [(80 - 10 * t, y), (920 + 10 * t, y), (920 + 10 * t, y + s), (80 - 10 * t, y + s)])
    poly(pen, [(470, 40), (470 + s, 40), (470 + s, 960), (470, 960)])
    return 1000

ORDER = list(GLYPHS)
CMAP = {cp: n for n, (cp, f) in GLYPHS.items() if cp}

def build_master(path, t, weight_class):
    fb = FontBuilder(1000, isTTF=False)
    fb.setupGlyphOrder(ORDER)
    fb.setupCharacterMap(CMAP)
    cs, metrics = {}, {}
    for n in ORDER:
        cp, draw = GLYPHS[n]
        probe = T2CharStringPen(0, None)
        adv = draw(probe, t)
        pen = T2CharStringPen(round(adv), None)
        draw(pen, t)
        c = pen.getCharString()
        if n == "H":
            # stems: horizontal stems at the bar, vertical at the stems; then a hintmask (2 hints sets)
            s = 60 + 140 * t
            width = [c.program[0]]   # defaultWidthX is 0, so the width is always the first number
            body = c.program[len(width):]
            hs = [round(300 - s / 2), round(s)]
            vs = [80, round(s), round(460 - s), round(s)]
            c.program = width + hs + ['hstemhm'] + vs + ['hintmask', bytes([0b11000000])] + body
        cs[n] = c
        metrics[n] = (round(adv), 0)
    fb.setupCFF("TestVF-%d" % weight_class, {"FullName": "Test VF %d" % weight_class}, cs, {"defaultWidthX": 0, "nominalWidthX": 0})
    fb.setupHorizontalMetrics(metrics)
    fb.setupHorizontalHeader(ascent=900, descent=-250)
    fb.setupNameTable({"familyName": "Test VF", "styleName": "W%d" % weight_class})
    fb.setupOS2(usWeightClass=weight_class, sTypoAscender=900, sTypoDescender=-250, sxHeight=500, sCapHeight=700)
    fb.setupPost()
    fb.save(path)

def make_vf(outname, default, masters, amap):
    tmp = tempfile.mkdtemp()
    ds = DesignSpaceDocument()
    ax = AxisDescriptor()
    ax.name, ax.tag, ax.minimum, ax.default, ax.maximum = "weight", "wght", 100, default, 900
    if amap:
        ax.map = amap
    ds.addAxis(ax)
    for w in masters:
        p = os.path.join(tmp, "m%d.otf" % w)
        build_master(p, (w - 100) / 800.0, w)
        s = SourceDescriptor()
        s.path = p
        s.location = {"weight": w}
        s.name = "m%d" % w
        ds.addSource(s)
    vf, _, _ = varLib.build(ds)
    vf["name"].setName("Test VF", 1, 3, 1, 0x409)
    path = os.path.join(OUT, outname)
    vf.save(path)
    return path

def dump(f, path, weights):
    font = TTFont(path)
    cmap = font.getBestCmap()
    f.write("font %s\n" % os.path.basename(path))
    for w in weights:
        gs = font.getGlyphSet(location={"wght": w})
        f.write("at %s\n" % w)
        for cp in sorted(cmap):
            name = cmap[cp]
            g = gs[name]
            rp = RecordingPen()
            g.draw(rp)
            f.write("glyph %d %.4f\n" % (cp, g.width))
            for op, args in rp.value:
                if op == "moveTo":
                    f.write("M %.4f %.4f\n" % args[0])
                elif op == "lineTo":
                    f.write("L %.4f %.4f\n" % args[0])
                elif op == "curveTo":
                    assert len(args) == 3, args
                    f.write("C " + " ".join("%.4f %.4f" % p for p in args) + "\n")
                elif op in ("closePath", "endPath"):
                    f.write("Z\n")
                else:
                    raise SystemExit("unexpected pen op %s" % op)

def main():
    os.makedirs(OUT, exist_ok=True)
    light = make_vf("vf_light.otf", 100, [100, 900], None)
    # user 700 sits at design 550 and 250 at 200: avar bends the axis between the masters
    regular = make_vf("vf_regular.otf", 400, [100, 400, 900], [(100, 100), (250, 200), (400, 400), (700, 550), (900, 900)])
    with open(os.path.join(OUT, "cff2_ref.txt"), "w") as f:
        dump(f, light, [50, 100, 250, 400, 700, 900, 1000])
        dump(f, regular, [100, 250, 400, 550, 700, 900])
    for p in (light, regular):
        t = TTFont(p)
        print(os.path.basename(p), os.path.getsize(p), "bytes", sorted(t.keys()))

main()
