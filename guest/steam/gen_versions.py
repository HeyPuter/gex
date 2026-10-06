#!/usr/bin/env python3
"""Generate gex-steam's interface classes for every interface version, from
a directory of Steamworks interface headers (docs/games.md, "gex-steam").

    gen_versions.py SDK_DIR STEAM_API_JSON ABI OUT.inc

SDK_DIR holds the current SDK headers (isteamuser.h, ...) and one header
per older interface version (isteamuser018.h declaring ISteamUser018, as
the Goldberg emulator's sdk/steam keeps them). STEAM_API_JSON (the current
SDK's steam_api.json) gives typedefs and struct layouts. ABI is msvc or
itanium.

Unlike gen.py (one SDK, callers clean the stack), this is for Windows i386,
where interface methods are __thiscall: the callee pops its arguments, so
every method is declared with its real parameters, and an old game asking
for SteamUser018 needs SteamUser018's vtable, not the newest one. A game
replaced with this library is one the original steam_api.dll was built for,
so the library answers every version a game of any age asks for.

Vtable order: the header's declaration order (STEAM_PRIVATE_API methods
included), except that for msvc overloads are grouped at the first one's
slot in reverse declaration order, as MSVC lays them out.
"""
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

PRIMS = {
    'void': 'void', 'bool': 'bool', 'float': 'float', 'double': 'double', 'char': 'char',
    'int': 'int', 'unsigned int': 'unsigned', 'unsigned': 'unsigned', 'short': 'short',
    'unsigned short': 'unsigned short', 'signed char': 'signed char', 'unsigned char': 'unsigned char',
    'long long': 'long long', 'unsigned long long': 'unsigned long long', 'long': 'int',
    'unsigned long': 'unsigned', 'int8': 'signed char', 'uint8': 'unsigned char', 'int16': 'short',
    'uint16': 'unsigned short', 'int32': 'int', 'uint32': 'unsigned', 'int64': 'long long',
    'uint64': 'unsigned long long', 'intptr_t': 'intptr_t', 'intp': 'intptr_t', 'uintp': 'uintptr_t',
    'size_t': 'uintptr_t', 'lint64': 'long long', 'ulint64': 'unsigned long long',
    'uint64_t': 'unsigned long long', 'int64_t': 'long long', 'uint32_t': 'unsigned', 'int32_t': 'int',
    'uint16_t': 'unsigned short', 'uint8_t': 'unsigned char', 'unknown_ret': 'int',
    'SteamNetworkingMicroseconds': 'long long', 'SteamNetworkingPOPID': 'unsigned',
    'HSteamListenSocket': 'unsigned', 'HSteamNetConnection': 'unsigned', 'HSteamNetPollGroup': 'unsigned',
}

# Classes passed or returned by value whose layout the JSON does not give.
SIZES = {'CSteamID': (8, 8), 'CGameID': (8, 8)}

# Bodies for the methods that say something, keyed by the interface's
# family (no version digits) and method; parameters are a0, a1, ... in
# declaration order, typed as below (a pointer to a primitive keeps its
# pointee, other pointers are void *, `char *` stays `char *`); $bool is
# the last `bool *` parameter (its position changed between versions).
OVERRIDES = {
    'ISteamUser::GetHSteamUser': 'return 1;',
    'ISteamUser::BLoggedOn': 'return true;',
    'ISteamUser::GetSteamID': 'return gex_steam_id();',
    'ISteamUser::BIsBehindNAT': 'return false;',
    'ISteamUser::GetUserDataFolder': 'return gex_steam_copy(a0, a1, gex_steam_dir());',
    'ISteamUser::GetAuthSessionTicket': 'int n = gex_steam_ticket(a0, a1); if (a2) *a2 = (unsigned)n; return n ? 1u : 0u;',
    'ISteamUser::GetPlayerSteamLevel': 'return 1;',
    'ISteamFriends::GetPersonaName': 'return gex_steam_name();',
    'ISteamFriends::GetFriendPersonaName': 'return gex_steam_name();',
    'ISteamFriends::GetPersonaState': 'return 1;',
    'ISteamUtils::GetAppID': 'return gex_steam_appid();',
    'ISteamUtils::GetIPCountry': 'return "US";',
    'ISteamUtils::GetSteamUILanguage': 'return "english";',
    'ISteamUtils::IsOverlayEnabled': 'return false;',
    'ISteamUtils::GetConnectedUniverse': 'return 1;',
    'ISteamUtils::GetServerRealTime': 'return gex_steam_time();',
    'ISteamUtils::GetSecondsSinceAppActive': 'return 0;',
    'ISteamUtils::GetSecondsSinceComputerActive': 'return 0;',
    'ISteamUtils::IsSteamRunningOnSteamDeck': 'return false;',
    'ISteamUtils::IsSteamInBigPictureMode': 'return false;',
    'ISteamUtils::GetCurrentBatteryPower': 'return 255;',
    'ISteamApps::BIsSubscribed': 'return true;',
    'ISteamApps::BIsSubscribedApp': 'return true;',
    'ISteamApps::BIsAppInstalled': 'return true;',
    'ISteamApps::BIsDlcInstalled': 'return gex_steam_dlc();',
    'ISteamApps::BIsLowViolence': 'return false;',
    'ISteamApps::BIsCybercafe': 'return false;',
    'ISteamApps::BIsVACBanned': 'return false;',
    'ISteamApps::GetCurrentGameLanguage': 'return "english";',
    'ISteamApps::GetAvailableGameLanguages': 'return "english";',
    'ISteamApps::GetAppOwner': 'return gex_steam_id();',
    'ISteamApps::GetAppBuildId': 'return 1;',
    'ISteamApps::GetCurrentBetaName': 'return false;',
    'ISteamApps::GetLaunchQueryParam': 'return "";',
    'ISteamApps::GetAppInstallDir': 'return gex_steam_copy(a1, (int)a2, gex_steam_dir()) ? (unsigned)gex_strlen(a1) : 0u;',
    'ISteamUserStats::RequestCurrentStats': 'return true;',
    'ISteamUserStats::GetAchievement': 'if ($bool) *$bool = false; return true;',
    'ISteamUserStats::SetAchievement': 'return true;',
    'ISteamUserStats::ClearAchievement': 'return true;',
    'ISteamUserStats::StoreStats': 'return true;',
    'ISteamUserStats::GetNumAchievements': 'return 0;',
    'ISteamRemoteStorage::IsCloudEnabledForAccount': 'return false;',
    'ISteamRemoteStorage::IsCloudEnabledForApp': 'return false;',
    'ISteamGameServer::BLoggedOn': 'return true;',
    'ISteamGameServer::BSecure': 'return false;',
    'ISteamGameServer::GetSteamID': 'return gex_steam_id();',
    'ISteamGameServer::GetAuthSessionTicket': 'int n = gex_steam_ticket(a0, a1); if (a2) *a2 = (unsigned)n; return n ? 1u : 0u;',
}

# ISteamClient's GetISteam*(user, pipe, version) and the generic lookup
# hand back this library's object for the version asked for.
CLIENT_GETTER = re.compile(r'GetISteam\w+|GetISteamGenericInterface')


def preprocess(text):
    """Comments out, then a tiny #if evaluator: only _WIN32/STEAM_WIN32 are
    defined (MSVC builds of the SDK), anything else is false."""
    text = re.sub(r'/\*.*?\*/', '', text, flags=re.S)
    text = re.sub(r'//[^\n]*', '', text)
    defined = {'_WIN32', 'STEAM_WIN32', 'WIN32'}

    def cond(expr):
        expr = expr.strip()
        if expr in ('0', '1'):
            return expr == '1'
        m = re.fullmatch(r'!?\s*defined\s*\(?\s*(\w+)\s*\)?', expr)
        if m:
            return (m.group(1) in defined) != expr.startswith('!')
        return False

    out, stack = [], []
    for line in text.split('\n'):
        s = line.strip()
        m = re.match(r'#\s*(ifdef|ifndef|if|elif|else|endif)\b(.*)', s)
        if m:
            kw, rest = m.group(1), m.group(2)
            if kw == 'ifdef':
                stack.append([rest.strip() in defined, False])
            elif kw == 'ifndef':
                # Include guards: everything else is "not defined".
                stack.append([rest.strip() not in defined, False])
            elif kw == 'if':
                stack.append([cond(rest), False])
            elif kw == 'elif':
                top = stack[-1]
                top[1] = top[1] or top[0]
                top[0] = not top[1] and cond(rest)
            elif kw == 'else':
                top = stack[-1]
                top[1] = top[1] or top[0]
                top[0] = not top[1]
            elif kw == 'endif':
                stack.pop()
            continue
        if all(t[0] for t in stack):
            out.append(line)
    return '\n'.join(out)


def strip_macros(s):
    """Drop the SDK's annotation macros (STEAM_OUT_STRING(), STEAM_ARRAY_COUNT(x), ...)."""
    prev = None
    while prev != s:
        prev = s
        s = re.sub(r'\bSTEAM_(?!PRIVATE_API)\w+\s*\((?:[^()]|\([^()]*\))*\)', ' ', s)
        s = re.sub(r'\bSTEAM_(?:METHOD_DESC|FLAT_NAME|IGNOREATTR|OUT_\w+|CALL_\w+)\b', ' ', s)
    return s


def split_top(s, sep=','):
    out, depth, cur = [], 0, ''
    for ch in s:
        if ch in '(<[':
            depth += 1
        elif ch in ')>]':
            depth -= 1
        if ch == sep and depth == 0:
            out.append(cur)
            cur = ''
        else:
            cur += ch
    if cur.strip():
        out.append(cur)
    return out


TYPE_WORDS = {'const', 'unsigned', 'signed', 'int', 'char', 'short', 'long', 'bool', 'float', 'double',
              'void', 'struct', 'class', 'enum', 'volatile'}


def parse_param(p):
    """(type, name) of one declared parameter; name may be ''."""
    p = p.split('=')[0].strip()
    if p == 'void' or not p:
        return None
    if '(*' in p:  # function pointer
        return ('void *', '')
    arr = ''
    m = re.search(r'\[[^\]]*\]\s*$', p)
    if m:
        arr, p = '*', p[:m.start()].strip()
    toks = re.findall(r'\w+|[*&]', p)
    if len(toks) >= 2 and re.match(r'\w+$', toks[-1]) and toks[-1] not in TYPE_WORDS and \
            not (all(t in TYPE_WORDS for t in toks[:-1]) and toks[-2] in ('unsigned', 'signed')):
        name, toks = toks[-1], toks[:-1]
    else:
        name = ''
    ty = ' '.join(toks).replace(' *', '*').replace('* ', '*') + arr
    return (ty, name)


def class_methods(body):
    """[(name, return type, [param types])] of a class body, in order."""
    body = re.sub(r'STEAM_PRIVATE_API\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)', r'\1', body)
    body = strip_macros(body)
    out = []
    for v in re.finditer(r'virtual\s+([^;{}()]*?)\b(\w+)\s*\(((?:[^()]|\((?:[^()]|\([^()]*\))*\))*)\)\s*(?:const\s*)?(?:=\s*0\s*)?;', body):
        ret, name, params = v.group(1).strip(), v.group(2), v.group(3)
        ps = [parse_param(p) for p in split_top(params)]
        out.append((name, ret, [p for p in ps if p]))
    return out


def main():
    sdk, jpath, abi, outp = sys.argv[1:5]
    api = json.load(open(jpath))
    typedefs = {t['typedef']: t['type'] for t in api['typedefs']}
    structs = {s['struct']: s for s in api['structs']}
    enums = {e['enumname'] for e in api['enums']}
    enums |= {e['enumname'] for i in api['interfaces'] for e in i.get('enums', [])}
    texts = {}
    for f in sorted(os.listdir(sdk)):
        if f.endswith('.h'):
            texts[f] = preprocess(open(os.path.join(sdk, f), errors='replace').read())
    alltext = '\n'.join(texts.values())
    for m in re.finditer(r'typedef\s+([\w\s\*]+?)\s+(\w+)\s*;', alltext):
        typedefs.setdefault(m.group(2), m.group(1).strip())
    for m in re.finditer(r'typedef\s+[\w\s\*]+\(\s*\w*\s*\*\s*(\w+)\s*\)', alltext):
        typedefs.setdefault(m.group(1), 'void *')
    for m in re.finditer(r'\benum\s+(\w+)', alltext):
        enums.add(m.group(1))
    # Structs the JSON does not know (older SDKs'): their fields from the
    # headers, as the JSON gives them.
    for m in re.finditer(r'\b(?:struct|class)\s+(\w+)\s*\{', alltext):
        if m.group(1) in structs or m.group(1).startswith('ISteam'):
            continue
        depth, i = 1, m.end()
        while depth and i < len(alltext):
            depth += {'{': 1, '}': -1}.get(alltext[i], 0)
            i += 1
        # Member functions' bodies become the end of their declaration.
        body, prev = alltext[m.end():i - 1], None
        while prev != body:
            prev = body
            body = re.sub(r'\{[^{}]*\}', ';', body)
        fields = []
        for decl in body.split(';'):
            decl = re.sub(r'\b(public|private|protected)\s*:', '', strip_macros(decl)).strip()
            if not decl or '(' in decl or decl.startswith(('enum', 'static', 'public', 'private')):
                continue
            pm = re.fullmatch(r'([\w\s\*:]+?)\s*\b(\w+)\s*(\[\s*(\w+)\s*\])?', decl, flags=re.S)
            if not pm:
                continue
            ty = pm.group(1).strip()
            if pm.group(4):
                n = pm.group(4)
                ty = f'{ty}[{n}]' if n.isdigit() else ty + '*'
            fields.append({'fieldtype': ty})
        if fields:
            structs[m.group(1)] = {'struct': m.group(1), 'fields': fields}
    # Current version strings: STEAMUSER_INTERFACE_VERSION "SteamUser023".
    current = {}
    for m in re.finditer(r'#define\s+(\w+_INTERFACE_VERSION\w*)\s+"([^"]+)"', '\n'.join(
            open(os.path.join(sdk, f), errors='replace').read() for f in texts)):
        current[m.group(1)] = m.group(2)

    def resolve(ty):
        seen = 0
        ty = ty.replace('const ', '').replace('struct ', '').replace('class ', '').replace('enum ', '').strip()
        while ty in typedefs and seen < 10 and ty not in PRIMS:
            ty = typedefs[ty].replace('const ', '').strip()
            seen += 1
        return ty

    def layout(ty, x64):
        ty = resolve(ty)
        if ty.endswith('*') or ty.endswith('&') or '(*)' in ty:
            return (8, 8) if x64 else (4, 4)
        if '[' in ty:
            base, n = ty[:ty.index('[')], ty[ty.index('[') + 1:ty.index(']')]
            s, a = layout(base, x64)
            return s * int(n), a
        if ty in SIZES:
            return SIZES[ty][1 if x64 else 0], 8 if x64 else 4
        if ty in structs:
            off, al = 0, 1
            for f in structs[ty]['fields']:
                s, a = layout(f['fieldtype'], x64)
                off = (off + a - 1) // a * a + s
                al = max(al, a)
            return (off + al - 1) // al * al, al
        c = PRIMS.get(ty)
        if c in ('long long', 'unsigned long long', 'double'):
            return 8, 8 if x64 else 4
        if c in ('intptr_t', 'uintptr_t'):
            return (8, 8) if x64 else (4, 4)
        if c in ('short', 'unsigned short'):
            return 2, 2
        if c in ('char', 'signed char', 'unsigned char', 'bool'):
            return 1, 1
        if c or ty in enums or ty.split('::')[-1] in enums:
            return 4, 4
        raise KeyError(ty)

    aggs = {}

    def agg(ty):
        name = 'gex_agg_' + re.sub(r'\W', '_', ty)
        aggs[name] = (layout(ty, False), layout(ty, True))
        return name

    def ctype(ty, is_ret=False):
        """The C type standing in for an SDK type."""
        raw = ty.strip()
        if raw.endswith('&'):
            raw = raw[:-1] + '*'
        if raw.endswith('*'):
            base = raw[:-1].replace('const', '').strip()
            if base == 'char':
                return 'const char *' if 'const' in raw else 'char *'
            r = resolve(base)
            if r in PRIMS and not raw[:-1].strip().endswith('*'):
                return PRIMS[r] + ' *'
            return 'void *'
        r = resolve(raw)
        if r.endswith('*') or '(*)' in r:
            return 'void *'
        if r in PRIMS:
            return PRIMS[r]
        if r in SIZES or r in structs:
            return agg(r)
        if r in enums or r.split('::')[-1] in enums:
            return 'int'
        raise SystemExit(f'gen_versions.py: no ABI type for {ty!r} ({r!r})')

    def version_key(s):
        """SteamUser018 / STEAMUSERSTATS_INTERFACE_VERSION011 -> (family, n)."""
        k = re.sub(r'[^A-Za-z0-9]', '', s).upper().replace('INTERFACEVERSION', '')
        m = re.fullmatch(r'([A-Z]+?)(\d+)', k)
        return (m.group(1), int(m.group(2))) if m else None

    classes, table = [], {}
    for f, text in texts.items():
        for m in re.finditer(r'\bclass\s+(ISteam\w+)\s*(?::[^{;]*)?\{', text):
            cls = m.group(1)
            depth, i = 1, m.end()
            while depth and i < len(text):
                depth += {'{': 1, '}': -1}.get(text[i], 0)
                i += 1
            methods = class_methods(text[m.end():i])
            if not methods:
                continue
            fam = re.sub(r'\d+$', '', cls)
            if re.search(r'\d+$', cls):
                key = version_key(cls[1:])
            else:
                # The current version: its header's #define.
                vs = [v for d, v in current.items() if version_key(v) and version_key(v)[0] == re.sub(r'[^A-Z]', '', cls[1:].upper())]
                if not vs:
                    continue
                key = version_key(vs[0])
            if key is None or key in table:
                continue
            if abi == 'msvc':
                order, groups = [], {}
                for meth in methods:
                    if meth[0] in groups:
                        groups[meth[0]].insert(0, meth)
                    else:
                        groups[meth[0]] = [meth]
                        order.append(meth[0])
                methods = [mm for n in order for mm in groups[n]]
            gname = f'Gex_{key[0]}_{key[1]:03d}'
            lines = [f'struct {gname} {{']
            for j, (name, ret, params) in enumerate(methods):
                try:
                    rt = ctype(ret, True)
                    pts = [ctype(t) for t, _ in params]
                except KeyError as e:
                    raise SystemExit(f'gen_versions.py: {cls}::{name}: no layout for {e}')
                decl = ', '.join(f'{t} a{k}' for k, t in enumerate(pts))
                body = OVERRIDES.get(f'{fam}::{name}')
                if body and '$bool' in body:
                    bools = [k for k, t in enumerate(pts) if t == 'bool *']
                    body = body.replace('$bool', f'a{bools[-1]}') if bools else None
                if body is None and fam == 'ISteamClient' and CLIENT_GETTER.fullmatch(name) and pts and pts[-1] == 'const char *':
                    body = f'return gex_find(a{len(pts) - 1});'
                if body is None:
                    body = '' if rt == 'void' else ('return {};' if rt.startswith('gex_agg_') else f'return ({rt})0;')
                unused = ' '.join(f'(void)a{k};' for k in range(len(pts)))
                lines.append(f'    virtual {rt} m{j}_{name}({decl}) {{ {unused} {body} }}')
            lines.append('};')
            classes.append('\n'.join(lines))
            table[key] = gname
    out = ['/* Generated by guest/steam/gen_versions.py; do not edit. */', '']
    for name, ((s32, _), (s64, _)) in sorted(aggs.items()):
        out.append(f'struct {name} {{ unsigned char b[sizeof(void *) == 8 ? {s64} : {s32}]; }};')
    out.append('typedef struct gex_agg_CSteamID gex_ret_CSteamID;')
    out.append('static gex_ret_CSteamID gex_steam_id();')
    out.append('static int gex_steam_ticket(void *buf, int max);')
    out.append('static void *gex_find(const char *version);')
    out.append('')
    out += classes
    out.append('')
    out.append('/* Every version: family (normalised: SteamUser018 and')
    out.append(' * STEAMUSER_INTERFACE_VERSION018 are both USER... 18), number, class. */')
    out.append('#define GEX_STEAM_VERSIONS(X) \\')
    for (fam, n), g in sorted(table.items()):
        out.append(f'    X({g}, "{fam}", {n}) \\')
    out.append('')
    open(outp, 'w').write('\n'.join(out) + '\n')
    print(f'{len(table)} interface versions, {len(aggs)} aggregates', file=sys.stderr)


if __name__ == '__main__':
    main()
