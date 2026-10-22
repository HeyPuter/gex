#!/usr/bin/env python3
"""Generate gex-steam's interface classes from the Steamworks SDK's
steam_api.json.

    gen.py STEAM_API_JSON OUT.inc

Every interface becomes a class whose virtual functions are in the SDK's
order, so its vtable is the one a game compiled against that SDK indexes.
A method ignores its arguments (the callers clean the stack on both i386
and x86-64) and returns zero of its return type; what matters for the ABI
is the return: an aggregate is returned through a hidden pointer, so each
one gets a struct of its size. The methods with a fixed answer (the session's
user, the language, ...) are in OVERRIDES.
"""
import json
import os
import re
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
    # Authentication with no Steam to vouch: a ticket that names the user,
    # which this library's own game server side accepts. A game checks only
    # that one came back (GoldSrc: "STEAM certificate length error").
    'ISteamUser::InitiateGameConnection_DEPRECATED': ('void *blob, int max, unsigned long long server, unsigned ip, unsigned short port, bool secure', '(void)server; (void)ip; (void)port; (void)secure; return gex_steam_ticket(blob, max);'),
    'ISteamUser::GetAuthSessionTicket': ('void *ticket, int max, unsigned *len', 'int n = gex_steam_ticket(ticket, max); if (len) *len = (unsigned)n; return n ? 1u : 0u;'),
    'ISteamGameServer::GetAuthSessionTicket': ('void *ticket, int max, unsigned *len', 'int n = gex_steam_ticket(ticket, max); if (len) *len = (unsigned)n; return n ? 1u : 0u;'),
    'ISteamGameServer::SendUserConnectAndAuthenticate_DEPRECATED': ('unsigned ip, const void *blob, unsigned size, void *user', '(void)ip; (void)blob; (void)size; gex_ret_CSteamID id = gex_steam_id(); if (user) memcpy(user, id.b, sizeof id.b); return true;'),
    'ISteamGameServer::BLoggedOn': ('', 'return true;'),
    'ISteamGameServer::BSecure': ('', 'return false;'),
    'ISteamGameServer::GetSteamID': ('', 'return gex_steam_id();'),
    # Stats are there and empty: a game may quit when its request fails.
    'ISteamUserStats::RequestCurrentStats': ('', 'return true;'),
    'ISteamUserStats::StoreStats': ('', 'return true;'),
    'ISteamUserStats::SetAchievement': ('', 'return true;'),
    'ISteamUserStats::ClearAchievement': ('', 'return true;'),
    'ISteamUserStats::GetAchievement': ('const char *name, bool *got', '(void)name; if (got) *got = false; return true;'),
    'ISteamRemoteStorage::IsCloudEnabledForAccount': ('', 'return false;'),
    'ISteamRemoteStorage::IsCloudEnabledForApp': ('', 'return false;'),
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


def class_virtuals(headers, cls):
    """The virtual functions of `cls` in its header, in vtable order, as
    (name, return type, private): steam_api.json leaves out the
    STEAM_PRIVATE_API ones, which still take their vtable slots (an
    ISteamGameServer's InitGameServer: without it every later method is
    one slot early for a game built against that header)."""
    for text in headers:
        m = re.search(r'\bclass\s+' + cls + r'\b[^;{]*\{', text)
        if not m:
            continue
        depth, i = 1, m.end()
        while depth and i < len(text):
            depth += {'{': 1, '}': -1}.get(text[i], 0)
            i += 1
        body = text[m.end():i]
        out = []
        for v in re.finditer(r'(STEAM_PRIVATE_API\s*\(\s*)?virtual\s+([^;(]*?)\b(\w+)\s*\(', body):
            out.append((v.group(3), v.group(2).strip(), bool(v.group(1))))
        return out
    return []


def with_private_methods(itf, headers):
    """The interface's methods with its private ones in their slots."""
    methods = list(itf['methods'])
    virtuals = class_virtuals(headers, itf['classname'])
    if not any(p for _, _, p in virtuals):
        return methods
    out, j = [], 0
    for name, ret, private in virtuals:
        if j < len(methods) and methods[j]['methodname'] == name:
            out.append(methods[j])
            j += 1
        elif private:
            out.append({'methodname': name, 'returntype': ret, 'params': []})
    return out + methods[j:]


def main():
    api = json.load(open(sys.argv[1]))
    # The SDK headers beside the JSON, for what it leaves out.
    hdir = os.path.dirname(os.path.abspath(sys.argv[1]))
    headers = []
    for f in sorted(os.listdir(hdir)):
        if f.endswith('.h'):
            t = open(os.path.join(hdir, f), errors='replace').read()
            t = re.sub(r'/\*.*?\*/', '', t, flags=re.S)
            headers.append(re.sub(r'//[^\n]*', '', t))
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
        for i, m in enumerate(with_private_methods(itf, headers)):
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
    out.append('static int gex_steam_ticket(void *buf, int max);')
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
