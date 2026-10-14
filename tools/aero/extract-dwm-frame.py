#!/usr/bin/env python3
"""Extract the Windows 7 DWM window-frame artwork from aero.msstyles.

    extract-dwm-frame.py AERO_MSSTYLES OUTDIR

Writes
  OUTDIR/dwm-atlas.png   the DWMWindow frame atlas, verbatim PNG bytes from the
                         STREAM resource that DWMWindow's IMAGEATLAS (213) names
  OUTDIR/dwm-parts.json  every DWMWindow part/state with its atlas rectangle,
                         sizing/content margins and other properties, plus the
                         theme's `globals` and `sysmetrics` classes

No dependencies beyond the Python 3 standard library.

msstyles format (Windows 7), as found in aero.msstyles:
  * resource "CMAP"/"CMAP": class names, UTF-16LE NUL-terminated strings, each
    padded to an 8-byte boundary; a property's class index indexes this list.
  * resource "VARIANT"/<name> (e.g. "NORMAL"): property records, each
        int32 nameId, typeId, classIdx, partId, stateId, shortFlag, reserved, size
    followed by `size` bytes of data padded to 8 -- unless shortFlag != 0, in
    which case the record is the 32-byte header alone and shortFlag holds a
    resource id (FILENAME 206 -> IMAGE id, FONT 210 -> string id, DISKSTREAM
    213 -> STREAM id).
  * The atlas: DWMWindow part 0 carries IMAGEATLAS (213, type DISKSTREAM) with
    shortFlag = id of a "STREAM" resource that is a PNG. Each part has
    ATLASRECT (8002, RECT left/top/right/bottom) and IMAGECOUNT (2401) images
    stacked vertically; image i is state i+1.
"""
import json
import os
import struct
import sys

# --- PE resources --------------------------------------------------------------


def pe_resources(data):
    """Return {(type, name, lang): bytes}; type/name are int ids or str names."""
    if data[:2] != b"MZ":
        raise ValueError("not a PE file")
    pe = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe:pe + 4] != b"PE\0\0":
        raise ValueError("no PE signature")
    nsec, = struct.unpack_from("<H", data, pe + 6)
    optsize, = struct.unpack_from("<H", data, pe + 20)
    opt = pe + 24
    magic, = struct.unpack_from("<H", data, opt)
    ddir = opt + (96 if magic == 0x10B else 112)
    rsrc_rva, _ = struct.unpack_from("<II", data, ddir + 2 * 8)
    secs = []
    for i in range(nsec):
        s = opt + optsize + i * 40
        vsize, va, rsize, rptr = struct.unpack_from("<IIII", data, s + 8)
        secs.append((va, max(vsize, rsize), rptr))

    def off(rva):
        for va, size, rptr in secs:
            if va <= rva < va + size:
                return rva - va + rptr
        raise ValueError("RVA %#x not in any section" % rva)

    base = off(rsrc_rva)

    def entry_name(v):
        if v & 0x80000000:
            p = base + (v & 0x7FFFFFFF)
            n, = struct.unpack_from("<H", data, p)
            return data[p + 2:p + 2 + 2 * n].decode("utf-16le")
        return v

    out = {}

    def walk(p, path, depth):
        nnamed, nid = struct.unpack_from("<HH", data, p + 12)
        for i in range(nnamed + nid):
            name, target = struct.unpack_from("<II", data, p + 16 + 8 * i)
            key = path + (entry_name(name),)
            if target & 0x80000000:
                if depth < 3:
                    walk(base + (target & 0x7FFFFFFF), key, depth + 1)
            else:
                rva, size = struct.unpack_from("<II", data, base + target)
                o = off(rva)
                out[key] = data[o:o + size]

    walk(base, (), 1)
    return out


def find_res(res, rtype, rname=None):
    for (t, n, _lang), blob in res.items():
        if t == rtype and (rname is None or n == rname):
            return blob
    return None


def string_table(res):
    """Win32 STRINGTABLE (type 6): {id: str}."""
    strings = {}
    for (t, block, _lang), blob in res.items():
        if t != 6 or not isinstance(block, int):
            continue
        p = 0
        for i in range(16):
            if p + 2 > len(blob):
                break
            n, = struct.unpack_from("<H", blob, p)
            p += 2
            if n:
                strings[(block - 1) * 16 + i] = blob[p:p + 2 * n].decode("utf-16le")
            p += 2 * n
    return strings


# --- msstyles ------------------------------------------------------------------


def parse_cmap(blob):
    names, p = [], 0
    while p + 2 <= len(blob):
        e = p
        while e + 2 <= len(blob) and blob[e:e + 2] != b"\0\0":
            e += 2
        names.append(blob[p:e].decode("utf-16le"))
        p = (e + 2 + 7) & ~7
    return names


def parse_variant(blob):
    recs, p = [], 0
    while p + 32 <= len(blob):
        name, typ, cls, part, state, short, _res, size = struct.unpack_from("<8i", blob, p)
        if short:
            recs.append((name, typ, cls, part, state, short, b""))
            p += 32
        else:
            recs.append((name, typ, cls, part, state, 0, blob[p + 32:p + 32 + size]))
            p += 32 + ((size + 7) & ~7)
    if p != len(blob):
        print("warning: VARIANT parse ended at %d of %d bytes" % (p, len(blob)), file=sys.stderr)
    return recs


# Property ids (vssym32.h TMT_* names; 213/8000-8002 per msstyleEditor).
PROP_NAMES = {
    213: "IMAGEATLAS", 401: "COLORSCHEMES", 402: "SIZES", 403: "CHARSET",
    801: "CAPTIONFONT", 802: "SMALLCAPTIONFONT", 803: "MENUFONT", 804: "STATUSFONT",
    805: "MSGBOXFONT", 806: "ICONTITLEFONT", 807: "HEADING1FONT", 808: "HEADING2FONT",
    809: "BODYFONT", 1001: "FLATMENUS",
    1201: "SIZINGBORDERWIDTH", 1202: "SCROLLBARWIDTH", 1203: "SCROLLBARHEIGHT",
    1204: "CAPTIONBARWIDTH", 1205: "CAPTIONBARHEIGHT", 1206: "SMCAPTIONBARWIDTH",
    1207: "SMCAPTIONBARHEIGHT", 1208: "MENUBARWIDTH", 1209: "MENUBARHEIGHT",
    1210: "PADDEDBORDERWIDTH", 1301: "MINCOLORDEPTH",
    1601: "SCROLLBAR", 1602: "BACKGROUND", 1603: "ACTIVECAPTION", 1604: "INACTIVECAPTION",
    1605: "MENU", 1606: "WINDOW", 1607: "WINDOWFRAME", 1608: "MENUTEXT", 1609: "WINDOWTEXT",
    1610: "CAPTIONTEXT", 1611: "ACTIVEBORDER", 1612: "INACTIVEBORDER", 1613: "APPWORKSPACE",
    1614: "HIGHLIGHT", 1615: "HIGHLIGHTTEXT", 1616: "BTNFACE", 1617: "BTNSHADOW",
    1618: "GRAYTEXT", 1619: "BTNTEXT", 1620: "INACTIVECAPTIONTEXT", 1621: "BTNHIGHLIGHT",
    1622: "DKSHADOW3D", 1623: "LIGHT3D", 1624: "INFOTEXT", 1625: "INFOBK",
    1626: "BUTTONALTERNATEFACE", 1627: "HOTTRACKING", 1628: "GRADIENTACTIVECAPTION",
    1629: "GRADIENTINACTIVECAPTION", 1630: "MENUHILIGHT", 1631: "MENUBAR",
    2201: "TRANSPARENT", 2202: "AUTOSIZE", 2203: "BORDERONLY", 2204: "COMPOSITED",
    2205: "BGFILL", 2206: "GLYPHTRANSPARENT", 2207: "GLYPHONLY", 2208: "ALWAYSSHOWSIZINGBAR",
    2209: "MIRRORIMAGE", 2210: "UNIFORMSIZING", 2211: "INTEGRALSIZING", 2212: "SOURCEGROW",
    2213: "SOURCESHRINK", 2214: "DRAWBORDERS", 2215: "NOETCHEDEFFECT", 2216: "TEXTAPPLYOVERLAY",
    2217: "TEXTGLOW", 2218: "TEXTITALIC", 2219: "COMPOSITEDOPAQUE", 2220: "LOCALIZEDMIRRORIMAGE",
    2401: "IMAGECOUNT", 2402: "ALPHALEVEL", 2403: "BORDERSIZE", 2404: "ROUNDCORNERWIDTH",
    2405: "ROUNDCORNERHEIGHT", 2406: "GRADIENTRATIO1", 2407: "GRADIENTRATIO2",
    2408: "GRADIENTRATIO3", 2409: "GRADIENTRATIO4", 2410: "GRADIENTRATIO5",
    2411: "PROGRESSCHUNKSIZE", 2412: "PROGRESSSPACESIZE", 2413: "SATURATION",
    2414: "TEXTBORDERSIZE", 2415: "ALPHATHRESHOLD", 2416: "WIDTH", 2417: "HEIGHT",
    2418: "GLYPHINDEX", 2419: "TRUESIZESTRETCHMARK", 2420: "MINDPI1", 2421: "MINDPI2",
    2422: "MINDPI3", 2423: "MINDPI4", 2424: "MINDPI5", 2425: "TEXTGLOWSIZE",
    2426: "FRAMESPERSECOND", 2427: "PIXELSPERFRAME", 2428: "ANIMATIONDELAY",
    2429: "GLOWINTENSITY", 2430: "OPACITY", 2431: "COLORIZATIONCOLOR",
    2432: "COLORIZATIONOPACITY", 2433: "MINDPI6", 2434: "MINDPI7", 2601: "GLYPHFONT",
    3001: "IMAGEFILE", 3002: "IMAGEFILE1", 3003: "IMAGEFILE2", 3004: "IMAGEFILE3",
    3005: "IMAGEFILE4", 3006: "IMAGEFILE5", 3008: "GLYPHIMAGEFILE", 3009: "IMAGEFILE6",
    3010: "IMAGEFILE7", 3201: "TEXT", 3202: "CLASSICVALUE",
    3401: "OFFSET", 3402: "TEXTSHADOWOFFSET", 3403: "MINSIZE", 3404: "MINSIZE1",
    3405: "MINSIZE2", 3406: "MINSIZE3", 3407: "MINSIZE4", 3408: "MINSIZE5", 3409: "NORMALSIZE",
    3601: "SIZINGMARGINS", 3602: "CONTENTMARGINS", 3603: "CAPTIONMARGINS",
    3801: "BORDERCOLOR", 3802: "FILLCOLOR", 3803: "TEXTCOLOR", 3804: "EDGELIGHTCOLOR",
    3805: "EDGEHIGHLIGHTCOLOR", 3806: "EDGESHADOWCOLOR", 3807: "EDGEDKSHADOWCOLOR",
    3808: "EDGEFILLCOLOR", 3809: "TRANSPARENTCOLOR", 3810: "GRADIENTCOLOR1",
    3811: "GRADIENTCOLOR2", 3812: "GRADIENTCOLOR3", 3813: "GRADIENTCOLOR4",
    3814: "GRADIENTCOLOR5", 3815: "SHADOWCOLOR", 3816: "GLOWCOLOR", 3817: "TEXTBORDERCOLOR",
    3818: "TEXTSHADOWCOLOR", 3819: "GLYPHTEXTCOLOR", 3820: "GLYPHTRANSPARENTCOLOR",
    3821: "FILLCOLORHINT", 3822: "BORDERCOLORHINT", 3823: "ACCENTCOLORHINT",
    3824: "TEXTCOLORHINT", 3825: "HEADING1TEXTCOLOR", 3826: "HEADING2TEXTCOLOR",
    3827: "BODYTEXTCOLOR",
    4001: "BGTYPE", 4002: "BORDERTYPE", 4003: "FILLTYPE", 4004: "SIZINGTYPE", 4005: "HALIGN",
    4006: "CONTENTALIGNMENT", 4007: "VALIGN", 4008: "OFFSETTYPE", 4009: "ICONEFFECT",
    4010: "TEXTSHADOWTYPE", 4011: "IMAGELAYOUT", 4012: "GLYPHTYPE", 4013: "IMAGESELECTTYPE",
    4014: "GLYPHFONTSIZINGTYPE", 4015: "TRUESIZESCALINGTYPE",
    5001: "USERPICTURE", 5002: "DEFAULTPANESIZE", 5003: "BLENDCOLOR", 5004: "CUSTOMSPLITRECT",
    5005: "ANIMATIONBUTTONRECT", 5006: "ANIMATIONDURATION", 6000: "TRANSITIONDURATIONS",
    7001: "SCALEDBACKGROUND", 8000: "ATLASIMAGE", 8001: "ATLASINPUTIMAGE", 8002: "ATLASRECT",
}

# DWMWindow part names for Windows 7, after msstyleEditor (nptr/msstyleEditor,
# VisualStylePartMap.cs, PARTS_DWMWINDOW_WIN7, names from a Win8 build 7762 PDB),
# and cross-checked against the artwork in the atlas.
DWM_PARTS = {
    1: "BOTTOMFRAME", 2: "BOTTOMSHADOW", 3: "BUTTONACTIVECAPTION",
    4: "BUTTONINACTIVECAPTION", 5: "BUTTONACTIVECAPTIONEND", 6: "BUTTONINACTIVECAPTIONEND",
    7: "BUTTONACTIVECLOSE", 8: "BUTTONINACTIVECLOSE", 9: "BUTTONACTIVECLOSEALONE",
    10: "BUTTONINACTIVECLOSEALONE", 11: "BUTTONCLOSEGLOW",
    12: "BUTTONCLOSEGLYPH96", 13: "BUTTONCLOSEGLYPH120", 14: "BUTTONCLOSEGLYPH144",
    15: "BUTTONCLOSEGLYPH192", 16: "BUTTONGLOW",
    17: "BUTTONHELPGLYPH96", 18: "BUTTONHELPGLYPH120", 19: "BUTTONHELPGLYPH144",
    20: "BUTTONHELPGLYPH192",
    21: "BUTTONMAXGLYPH96", 22: "BUTTONMAXGLYPH120", 23: "BUTTONMAXGLYPH144",
    24: "BUTTONMAXGLYPH192",
    25: "BUTTONMINGLYPH96", 26: "BUTTONMINGLYPH120", 27: "BUTTONMINGLYPH144",
    28: "BUTTONMINGLYPH192",
    29: "BUTTONRESTOREGLYPH96", 30: "BUTTONRESTOREGLYPH120", 31: "BUTTONRESTOREGLYPH144",
    32: "BUTTONRESTOREGLYPH192",
    33: "LEFTFRAME", 34: "LEFTHIGHLIGHTCORNER", 35: "LEFTSHADOW",
    36: "FLIP3DBOTTOMFRAME", 37: "FLIP3DLEFTFRAME", 38: "FLIP3DRIGHTFRAME",
    39: "FLIP3DTOPFRAME", 40: "REFLECTIONMAP", 41: "RIGHTFRAME", 42: "RIGHTSHADOW",
    43: "SIDEHIGHLIGHT", 44: "SMALLBOTTOMFRAME", 45: "SMALLBUTTONACTIVECLOSE",
    46: "SMALLBUTTONINACTIVECLOSE", 47: "SMALLBUTTONCLOSEGLOW",
    48: "SMALLBUTTONCLOSEGLYPH96", 49: "SMALLBUTTONCLOSEGLYPH120",
    50: "SMALLBUTTONCLOSEGLYPH144", 51: "SMALLBUTTONCLOSEGLYPH192",
    52: "SMALLLEFTFRAME", 53: "SMALLRIGHTFRAME", 54: "SMALLTOPFRAME", 55: "TEXTGLOW",
    56: "TOPFRAME", 57: "TOPSHADOW", 58: "TOPFRAMENOSHADOW", 59: "BOTTOMFRAMENOSHADOW",
    60: "LEFTFRAMESQUEEGEE", 61: "RIGHTFRAMESQUEEGEE", 62: "TOPFRAMESQUEEGEE",
    63: "BOTTOMFRAMESQUEEGEE", 64: "SMALLLEFTFRAMESQUEEGEE", 65: "SMALLRIGHTFRAMESQUEEGEE",
    66: "SMALLTOPFRAMESQUEEGEE", 67: "SMALLBOTTOMFRAMESQUEEGEE", 68: "BITMAPPENDING",
    69: "RIPPLE", 70: "TOPFRAMESQUEEGEENOSHADOW", 71: "BOTTOMFRAMESQUEEGEENOSHADOW",
}
FRAME_STATES = {1: "ACTIVE", 2: "INACTIVE"}
BUTTON_STATES = {1: "NORMAL", 2: "HOT", 3: "PUSHED", 4: "DISABLED"}
# Parts whose stacked images are animation frames, not states.
ANIMATION_PARTS = {68, 69}


def state_name(part, state):
    if state == 0:
        return "COMMON"
    name = DWM_PARTS.get(part, "")
    if "BUTTON" in name and "GLOW" not in name:
        return BUTTON_STATES.get(state)
    if "FRAME" in name or "SHADOW" in name or "HIGHLIGHTCORNER" in name:
        return FRAME_STATES.get(state)
    return None


def colorref(v):
    return "#%02x%02x%02x" % (v & 0xFF, (v >> 8) & 0xFF, (v >> 16) & 0xFF)


def decode(typ, short, data, strings):
    """Decode one property value into JSON-friendly form."""
    if short:
        if typ == 210:
            return {"fontStringId": short, "font": strings.get(short)}
        if typ == 206:
            return {"imageResourceId": short}
        if typ == 213:
            return {"streamResourceId": short}
        return {"resourceId": short}
    n = len(data)
    if typ in (200, 202, 207) and n >= 4:
        return struct.unpack_from("<i", data)[0]
    if typ == 203 and n >= 4:
        return bool(struct.unpack_from("<i", data)[0])
    if typ == 204 and n >= 4:
        return colorref(struct.unpack_from("<I", data)[0])
    if typ == 205 and n >= 16:
        return list(struct.unpack_from("<4i", data))  # cxLeft, cxRight, cyTop, cyBottom
    if typ == 208 and n >= 8:
        return list(struct.unpack_from("<2i", data))
    if typ == 209 and n >= 16:
        l, t, r, b = struct.unpack_from("<4i", data)
        return [l, t, r - l, b - t]  # x, y, w, h
    if typ == 211 and n >= 4:
        c = struct.unpack_from("<i", data)[0]
        return list(struct.unpack_from("<%di" % c, data, 4))
    if typ == 216 and n >= 4:
        return struct.unpack_from("<f", data)[0]
    if typ == 217 and n >= 4:
        c = struct.unpack_from("<i", data)[0]
        return list(struct.unpack_from("<%df" % c, data, 4))
    if typ == 201:
        return data.decode("utf-16le").rstrip("\0")
    return {"type": typ, "hex": data.hex()}


def prop_key(name_id):
    return PROP_NAMES.get(name_id, "PROP_%d" % name_id)


def class_props(recs, cls, strings):
    """{(part, state): {propName: value}} for one class."""
    out = {}
    for name, typ, c, part, state, short, data in recs:
        if c != cls:
            continue
        v = decode(typ, short, data, strings)
        if name == 2431 and isinstance(v, int):  # COLORIZATIONCOLOR: int used as ARGB
            v = {"value": v, "argb": "#%08x" % (v & 0xFFFFFFFF)}
        out.setdefault((part, state), {})[prop_key(name)] = v
    return out


def main(argv):
    if len(argv) != 3:
        print(__doc__.strip().splitlines()[2], file=sys.stderr)
        return 2
    src, outdir = argv[1], argv[2]
    with open(src, "rb") as f:
        data = f.read()
    res = pe_resources(data)

    strings = string_table(res)
    # Fonts and other strings live in the language .mui next to the theme.
    d, base = os.path.split(os.path.abspath(src))
    for sub in sorted(os.listdir(d)) if os.path.isdir(d) else []:
        mui = os.path.join(d, sub, base + ".mui")
        if os.path.isfile(mui):
            with open(mui, "rb") as f:
                for k, v in string_table(pe_resources(f.read())).items():
                    strings.setdefault(k, v)
            break

    cmap = parse_cmap(find_res(res, "CMAP", "CMAP"))
    variant_name = "NORMAL"
    vblob = find_res(res, "VARIANT", variant_name)
    if vblob is None:
        for (t, n, _l) in res:
            if t == "VARIANT":
                variant_name, vblob = n, find_res(res, "VARIANT", n)
                break
    recs = parse_variant(vblob)
    try:
        dwm = cmap.index("DWMWindow")
    except ValueError:
        print("no DWMWindow class in CMAP", file=sys.stderr)
        return 1

    props = class_props(recs, dwm, strings)
    atlas_ref = props.get((0, 0), {}).get("IMAGEATLAS")
    if not atlas_ref:
        print("DWMWindow has no IMAGEATLAS property", file=sys.stderr)
        return 1
    stream_id = atlas_ref["streamResourceId"]
    png = find_res(res, "STREAM", stream_id)
    if png is None or png[:8] != b"\x89PNG\r\n\x1a\n":
        print("STREAM %r missing or not a PNG" % stream_id, file=sys.stderr)
        return 1
    aw, ah = struct.unpack(">II", png[16:24])

    os.makedirs(outdir, exist_ok=True)
    with open(os.path.join(outdir, "dwm-atlas.png"), "wb") as f:
        f.write(png)

    parts = []
    for part in sorted({p for p, _s in props if p}):
        base_props = props.get((part, 0), {})
        rect = base_props.get("ATLASRECT")
        count = base_props.get("IMAGECOUNT") or 1
        frames = None
        if rect:
            fh = rect[3] // count
            frames = [[rect[0], rect[1] + i * fh, rect[2], fh] for i in range(count)]
        states = {s for p, s in props if p == part} | {0}
        if part not in ANIMATION_PARTS and count > 1:
            states |= set(range(1, count + 1))
        for state in sorted(states):
            merged = dict(base_props)
            merged.update(props.get((part, state), {}))  # state falls back to part
            e = {"part": part, "partName": DWM_PARTS.get(part), "state": state,
                 "stateName": state_name(part, state)}
            if state == 0:
                e["rect"] = rect
                e["imageCount"] = count
                if frames and count > 1:
                    e["frames"] = frames
            else:
                # A state past IMAGECOUNT (e.g. TOPFRAME 3/4, which only add
                # TEXTCOLOR) has no image of its own: rect is null.
                e["rect"] = (frames[state - 1] if state <= count else None) if frames else None
            e["sizingMargins"] = merged.pop("SIZINGMARGINS", None)
            e["contentMargins"] = merged.pop("CONTENTMARGINS", None)
            for k in ("ATLASRECT", "IMAGECOUNT"):
                merged.pop(k, None)
            if "BGTYPE" in merged:
                e["bgType"] = merged.pop("BGTYPE")
            e["properties"] = merged
            parts.append(e)

    def flat(cls_name):
        if cls_name not in cmap:
            return None
        return {k: v for (_p, _s), d in sorted(class_props(recs, cmap.index(cls_name), strings).items())
                for k, v in d.items()}

    out = {
        "source": os.path.basename(src),
        "variant": variant_name,
        "atlas": {"file": "dwm-atlas.png", "streamResourceId": stream_id, "size": [aw, ah]},
        "notes": [
            "rect is [x, y, w, h] in the atlas; margins are [left, right, top, bottom].",
            "state 0 rect covers all stacked images; state N uses image N-1 (stacked vertically);"
            " a state with no image of its own (N > imageCount) has rect null.",
            "state entries inherit part (state 0) properties, as GetThemeXxx falls back.",
            "BITMAPPENDING (68) and RIPPLE (69) images are animation frames, see 'frames'.",
            "colors are COLORREF rendered as #rrggbb; COLORIZATIONCOLOR is an int used as ARGB.",
        ],
        "common": props.get((0, 0), {}),
        "parts": parts,
        "globals": flat("globals"),
        "sysmetrics": flat("sysmetrics"),
    }
    with open(os.path.join(outdir, "dwm-parts.json"), "w") as f:
        json.dump(out, f, indent=1)
        f.write("\n")
    print("atlas %dx%d (STREAM %s), %d part/state entries -> %s"
          % (aw, ah, stream_id, len(parts), outdir))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
