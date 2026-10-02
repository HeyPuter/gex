#!/usr/bin/env python3
"""Generate gex-steam's interface classes from the Steamworks SDK's
steam_api.json (docs/games.md).

    gen.py STEAM_API_JSON OUT.inc

Every interface becomes a class whose virtual functions are in the SDK's
order, so its vtable is the one a game compiled against that SDK indexes.
A method ignores its arguments (the callers clean the stack on both i386
and x86-64) and returns zero of its return type; what matters for the ABI
is the return: an aggregate is returned through a hidden pointer, so each
one gets a struct of its size. The methods with an answer worth giving
(logged on, owns the app, the language) are in OVERRIDES.
"""
import json
import sys

# Bodies for the methods that say something. Keyed "Class::Method"; the
# value is (parameters, body). Parameters are only declared when the body
# reads them.
OVERRIDES = {
    'ISteamUser::GetHSteamUser': ('', 'return 1;'),
    'ISteamUser::BLoggedOn': ('', 'return true;'),
    'ISteamUser::GetSteamID': ('', 'return gex_steam_id();'),
    'ISteamUser::BIsBehindNAT': ('', 'return false;'),
    'ISteamUser::GetUserDataFolder': ('char *buf, int size', 'return gex_steam_copy(buf, size, gex_steam_dir());'),
    'ISteamFriends::GetPersonaName': ('', 'return gex_steam_name();'),
    'ISteamFriends::GetFriendPersonaName': ('', 'return gex_steam_name();'),
    'ISteamFriends::GetPersonaState': ('', 'return 1;'),  # k_EPersonaStateOnline
    'ISteamUtils::GetAppID': ('', 'return gex_steam_appid();'),
    'ISteamUtils::GetIPCountry': ('', 'return "US";'),
    'ISteamUtils::GetSteamUILanguage': ('', 'return "english";'),
    'ISteamUtils::IsOverlayEnabled': ('', 'return false;'),
    'ISteamUtils::GetConnectedUniverse': ('', 'return 1;'),  # k_EUniversePublic
    'ISteamUtils::GetServerRealTime': ('', 'return gex_steam_time();'),
    'ISteamUtils::GetSecondsSinceAppActive': ('', 'return 0;'),
    'ISteamApps::BIsSubscribed': ('', 'return true;'),
    'ISteamApps::BIsSubscribedApp': ('', 'return true;'),
    'ISteamApps::BIsAppInstalled': ('', 'return true;'),
    'ISteamApps::BIsLowViolence': ('', 'return false;'),
    'ISteamApps::BIsCybercafe': ('', 'return false;'),
    'ISteamApps::BIsVACBanned': ('', 'return false;'),
    'ISteamApps::GetCurrentGameLanguage': ('', 'return "english";'),
    'ISteamApps::GetAvailableGameLanguages': ('', 'return "english";'),
    'ISteamApps::GetAppOwner': ('', 'return gex_steam_id();'),
    'ISteamApps::GetAppBuildId': ('', 'return 1;'),
    'ISteamApps::GetCurrentBetaName': ('', 'return false;'),
    'ISteamApps::GetLaunchQueryParam': ('', 'return "";'),
    'ISteamApps::GetAppInstallDir': ('unsigned app, char *buf, unsigned size', '(void)app; return gex_steam_copy(buf, (int)size, gex_steam_dir());'),
    'ISteamGameServer::BLoggedOn': ('', 'return true;'),
    'ISteamGameServer::BSecure': ('', 'return false;'),
    'ISteamGameServer::GetSteamID': ('', 'return gex_steam_id();'),
}

# Aggregates returned by value: sizes for the i386 and x86-64 layouts.
AGGREGATES = {'CSteamID': 8, 'CGameID': 8}

PRIMS = {
    'void': 'void', 'bool': 'bool', 'float': 'float', 'double': 'double', 'char': 'char',
    'int': 'int', 'unsigned int': 'unsigned', 'short': 'short', 'unsigned short': 'unsigned short',
    'signed char': 'signed char', 'unsigned char': 'unsigned char',
    'long long': 'long long', 'unsigned long long': 'unsigned long long',
    'int8': 'signed char', 'uint8': 'unsigned char', 'int16': 'short', 'uint16': 'unsigned short',
    'int32': 'int', 'uint32': 'unsigned', 'int64': 'long long', 'uint64': 'unsigned long long',
    'intptr_t': 'long', 'intp': 'long', 'uintp': 'unsigned long',
    # Typedefs the JSON leaves out.
    'SteamNetworkingMicroseconds': 'long long', 'SteamNetworkingPOPID': 'unsigned',
    'HSteamListenSocket': 'unsigned', 'HSteamNetConnection': 'unsigned', 'HSteamNetPollGroup': 'unsigned',
}


def main():
    api = json.load(open(sys.argv[1]))
    typedefs = {t['typedef']: t['type'] for t in api['typedefs']}
    structs = {s['struct']: s for s in api['structs']}
    enums = {e['enumname'] for e in api['enums']}
    enums |= {e['enumname'] for i in api['interfaces'] for e in i.get('enums', [])}

    def layout(ty, x64):
        """(size, alignment) of a type as a struct field."""
        ty = ty.strip()
        if ty.endswith('*') or '(*)' in ty or ty.endswith('&'):
            return (8, 8) if x64 else (4, 4)
        if '[' in ty:
            base, n = ty[:ty.index('[')], ty[ty.index('[') + 1:ty.index(']')]
            s, a = layout(base, x64)
            return s * int(n), a
        if ty in AGGREGATES:
            return AGGREGATES[ty], 8 if x64 else 4
        if ty in structs:
            off, al = 0, 1
            for f in structs[ty]['fields']:
                s, a = layout(f['fieldtype'], x64)
                off = (off + a - 1) // a * a + s
                al = max(al, a)
            return (off + al - 1) // al * al, al
        c = PRIMS.get(ty) or PRIMS.get(typedefs.get(ty, ''))
        if c in ('long long', 'unsigned long long', 'double'):
            return 8, 8 if x64 else 4
        if c in ('long', 'unsigned long'):
            return (8, 8) if x64 else (4, 4)
        if c in ('short', 'unsigned short'):
            return 2, 2
        if c in ('char', 'signed char', 'unsigned char', 'bool'):
            return 1, 1
        return 4, 4  # int, float, enums

    out = ['/* Generated by guest/steam/gen.py from steam_api.json; do not edit. */', '']
    aggs = {}

    def ret_type(ty):
        ty = ty.strip()
        if ty.endswith('*') or ty.endswith('&'):
            return 'const char *' if ty.replace(' ', '') == 'constchar*' else 'void *'
        if ty in AGGREGATES or ty in structs:
            name = 'gex_ret_' + ty.replace(':', '_')
            aggs[name] = (layout(ty, False)[0], layout(ty, True)[0])
            return name
        c = PRIMS.get(ty) or PRIMS.get(typedefs.get(ty, ''))
        if c:
            return c
        if ty in enums or ty.split('::')[-1] in enums:
            return 'int'
        if typedefs.get(ty, '').startswith('void (*)') or typedefs.get(ty) == 'void *':
            return 'void *'
        raise SystemExit(f'gen.py: no ABI type for {ty!r}')

    classes = []
    for itf in api['interfaces']:
        cls = itf['classname']
        lines = [f'struct Gex{cls} {{']
        for i, m in enumerate(itf['methods']):
            rt = ret_type(m['returntype'])
            key = f"{cls}::{m['methodname']}"
            params, body = OVERRIDES.get(key, ('', None))
            if body is None:
                body = '' if rt == 'void' else ('return {};' if rt.startswith('gex_ret_') else f'return ({rt})0;')
            lines.append(f'    virtual {rt} m{i}_{m["methodname"]}({params}) {{ {body} }}')
        lines.append('};')
        classes.append('\n'.join(lines))
    for name, (s32, s64) in sorted(aggs.items()):
        out.append(f'struct {name} {{ unsigned char b[sizeof(void *) == 8 ? {s64} : {s32}]; }};')
    # The one helper an override needs that returns an aggregate.
    out.append('static gex_ret_CSteamID gex_steam_id();')
    out.append('')
    out += classes
    out.append('')
    # The table FindOrCreate*Interface looks versions up in.
    out.append('#define GEX_STEAM_INTERFACES(X) \\')
    for itf in api['interfaces']:
        if itf.get('version_string'):
            out.append(f'    X(Gex{itf["classname"]}, "{itf["version_string"]}") \\')
    out.append('')
    open(sys.argv[2], 'w').write('\n'.join(out) + '\n')


if __name__ == '__main__':
    main()
