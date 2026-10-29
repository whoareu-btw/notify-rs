#!/usr/bin/env python3
"""Reference numbers for tests/font_ref.rs, from fontTools (an independent implementation).

One line per glyph: path|face|codepoint|gid|advance|xmin|ymin|xmax|ymax|area
"""
import sys
from fontTools.ttLib import TTFont
from fontTools.pens.boundsPen import BoundsPen
from fontTools.pens.areaPen import AreaPen

FONTS = [
    ("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf", 0, "AaBgQ&@0%{}éñüÅ°±×÷ßŒ§¶©®™€£¥¢ ǆ"),
    ("/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf", 0, "Hamburgefonstiv ÄÖÜ%&@"),
    ("/usr/share/fonts/opentype/inter/Inter-Regular.otf", 0, "AaBgQ&@0%éñüÅ→★ fi"),
    ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 0, "日本語の曲あいうえおカタカナ漢字夜に駆ける一二三龍鬱ABCabc、。"),
    ("/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc", 0, "日本語曲中文한AaBb"),
    ("/usr/share/fonts/opentype/baskerville/GFSBaskerville.otf", 0, "AaBgQ&"),
]

out = []
for path, face, chars in FONTS:
    try:
        f = TTFont(path, fontNumber=face, lazy=True)
    except Exception as e:
        print("skip", path, e, file=sys.stderr)
        continue
    cmap = f.getBestCmap()
    gs = f.getGlyphSet()
    for ch in chars:
        cp = ord(ch)
        name = cmap.get(cp)
        if name is None:
            continue
        gid = f.getGlyphID(name)
        adv = f["hmtx"][name][0]
        bp = BoundsPen(gs)
        gs[name].draw(bp)
        ap = AreaPen(gs)
        gs[name].draw(ap)
        b = bp.bounds or (0, 0, 0, 0)
        out.append(f"{path}|{face}|{cp}|{gid}|{adv}|{b[0]:.3f}|{b[1]:.3f}|{b[2]:.3f}|{b[3]:.3f}|{ap.value:.3f}")
open("tests/fixtures/font_ref.txt", "w").write("\n".join(out) + "\n")
print(len(out), "glyphs")
