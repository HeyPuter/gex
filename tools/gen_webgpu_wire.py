#!/usr/bin/env python3
"""Generates the gex WebGPU wire from Dawn's API description.

The wire carries webgpu.h calls from a client (guest/gpu/wire-client.c, the
guest's webgpu.h) to a server (guest/gpu/wire-server.c, which makes them on
a real webgpu.h: Dawn natively, the browser's WebGPU through emdawnwebgpu).
docs/d3d-webgpu.md describes the protocol.

Input: third_party/dawn-api/dawn.json, the description Dawn generates its
own webgpu.h from (third_party/dawn-api/webgpu.h, the same revision); only
the standard API is carried, not Dawn's extensions (items tagged "dawn",
"native", ...).

Output (guest/gpu/gen/):
  wire-gen.h          opcodes, object types, the server's proc table
  wire-client-gen.inc   the client's serialisers and generated entry points
  wire-server-gen.inc   the server's deserialisers and dispatch
  wire-rename-gen.h   wgpuX -> gex_wgpuX, for a client linked beside a real
                      webgpu.h in one process (the tests)

Calls whose semantics need more than serialising arguments (callbacks and
futures, buffer mapping, getters answered by the client, reference counts)
are written by hand; HANDWRITTEN names them.
"""

import json
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
API = os.path.join(ROOT, 'third_party', 'dawn-api', 'dawn.json')
OUT = os.path.join(ROOT, 'guest', 'gpu', 'gen')

# Client entry points written by hand (guest/gpu/wire-client.c), by C name.
HANDWRITTEN_CLIENT = {
    'wgpuCreateInstance', 'wgpuGetProcAddress', 'wgpuGetInstanceFeatures', 'wgpuHasInstanceFeature',
    'wgpuGetInstanceLimits',
    'wgpuInstanceRequestAdapter', 'wgpuInstanceWaitAny', 'wgpuInstanceProcessEvents',
    'wgpuInstanceCreateSurface',
    'wgpuAdapterRequestDevice',
    'wgpuDeviceCreateBuffer', 'wgpuDeviceCreateTexture', 'wgpuDeviceCreateQuerySet',
    'wgpuDeviceCreateComputePipelineAsync', 'wgpuDeviceCreateRenderPipelineAsync',
    'wgpuDevicePopErrorScope', 'wgpuDeviceGetQueue', 'wgpuDeviceGetLostFuture', 'wgpuDeviceGetAdapter',
    'wgpuBufferMapAsync', 'wgpuBufferGetMappedRange', 'wgpuBufferGetConstMappedRange',
    'wgpuBufferReadMappedRange', 'wgpuBufferWriteMappedRange', 'wgpuBufferUnmap', 'wgpuBufferGetSize',
    'wgpuBufferGetUsage', 'wgpuBufferGetMapState', 'wgpuBufferDestroy',
    'wgpuQueueOnSubmittedWorkDone',
    'wgpuShaderModuleGetCompilationInfo',
    'wgpuTextureGetWidth', 'wgpuTextureGetHeight', 'wgpuTextureGetDepthOrArrayLayers',
    'wgpuTextureGetMipLevelCount', 'wgpuTextureGetSampleCount', 'wgpuTextureGetDimension',
    'wgpuTextureGetFormat', 'wgpuTextureGetUsage', 'wgpuTextureGetTextureBindingViewDimension',
    'wgpuQuerySetGetType', 'wgpuQuerySetGetCount',
    'wgpuSurfaceConfigure', 'wgpuSurfaceGetCapabilities', 'wgpuSurfaceGetCurrentTexture',
    'wgpuSurfacePresent', 'wgpuSurfaceUnconfigure',
    'wgpuAdapterInfoFreeMembers', 'wgpuSupportedFeaturesFreeMembers',
    'wgpuSupportedWGSLLanguageFeaturesFreeMembers', 'wgpuSupportedInstanceFeaturesFreeMembers',
    'wgpuSurfaceCapabilitiesFreeMembers',
}

# Server cases written by hand (guest/gpu/wire-server.c): the calls with
# callbacks.
HANDWRITTEN_SERVER = {
    'wgpuCreateInstance', 'wgpuInstanceRequestAdapter', 'wgpuInstanceCreateSurface',
    'wgpuAdapterRequestDevice', 'wgpuDeviceCreateComputePipelineAsync',
    'wgpuDeviceCreateRenderPipelineAsync', 'wgpuDevicePopErrorScope', 'wgpuBufferMapAsync',
    'wgpuQueueOnSubmittedWorkDone', 'wgpuShaderModuleGetCompilationInfo',
    'wgpuSurfaceConfigure', 'wgpuSurfaceGetCapabilities', 'wgpuSurfaceGetCurrentTexture',
    'wgpuSurfacePresent', 'wgpuSurfaceUnconfigure',
}

# Calls with no server side at all: the client answers them.
CLIENT_ONLY = {
    'wgpuBufferGetMappedRange', 'wgpuBufferGetConstMappedRange', 'wgpuBufferReadMappedRange',
    'wgpuBufferWriteMappedRange', 'wgpuBufferGetSize', 'wgpuBufferGetUsage', 'wgpuBufferGetMapState',
    'wgpuTextureGetWidth', 'wgpuTextureGetHeight', 'wgpuTextureGetDepthOrArrayLayers',
    'wgpuTextureGetMipLevelCount', 'wgpuTextureGetSampleCount', 'wgpuTextureGetDimension',
    'wgpuTextureGetFormat', 'wgpuTextureGetUsage', 'wgpuTextureGetTextureBindingViewDimension',
    'wgpuQuerySetGetType', 'wgpuQuerySetGetCount', 'wgpuGetProcAddress', 'wgpuDeviceGetQueue',
    'wgpuDeviceGetLostFuture', 'wgpuDeviceGetAdapter', 'wgpuInstanceWaitAny', 'wgpuInstanceProcessEvents',
    'wgpuAdapterInfoFreeMembers', 'wgpuSupportedFeaturesFreeMembers',
    'wgpuSupportedWGSLLanguageFeaturesFreeMembers', 'wgpuSupportedInstanceFeaturesFreeMembers',
    'wgpuSurfaceCapabilitiesFreeMembers', 'wgpuGetInstanceFeatures', 'wgpuHasInstanceFeature',
    'wgpuGetInstanceLimits',
}

NATIVE_32 = {'bool', 'uint8_t', 'uint16_t', 'uint32_t', 'int32_t', 'int'}
NATIVE_64 = {'uint64_t', 'size_t'}


def upstream(item):
    return isinstance(item, dict) and not (set(item.get('tags', [])) - {'upstream'})


def camel(name):
    """"texture view" -> "TextureView", as Dawn's generator spells it."""
    return ''.join(chunk[0].upper() + chunk[1:] for chunk in name.split(' '))


def lower_camel(name):
    chunks = name.split(' ')
    return chunks[0] + ''.join(chunk[0].upper() + chunk[1:] for chunk in chunks[1:])


def c_type(name):
    return 'WGPU' + camel(name)


class Api:
    def __init__(self, path):
        raw = json.load(open(path))
        self.items = {k: v for k, v in raw.items() if not k.startswith('_') and upstream(v)}
        self.objects = {k: v for k, v in self.items.items() if v['category'] == 'object'}
        self.structs = {k: v for k, v in self.items.items() if v['category'] == 'structure'}
        self.stypes = [v['name'] for v in self.items['s type']['values'] if upstream(v)]

    def category(self, type_name):
        if type_name in self.items:
            return self.items[type_name]['category']
        return 'native'

    def methods(self, obj_name):
        for m in self.objects[obj_name].get('methods', []):
            if upstream(m):
                yield m

    def functions(self):
        for k, v in self.items.items():
            if v['category'] == 'function':
                yield k, v


def native_c(t):
    return {'bool': 'WGPUBool'}.get(t, t)


def ret_type(m):
    """A method's return type: a name, or a dict with one."""
    r = m.get('returns')
    return r['type'] if isinstance(r, dict) else r


def member_c_name(member):
    return lower_camel(member['name'])


class Gen:
    def __init__(self, api):
        self.api = api
        self.ser_needed = set()     # structs the client serialises (in)
        self.out_needed = set()     # structs the server serialises back (out)
        self.ops = []               # (op enum name, C function, object or None, method dict)

    # ---- type helpers -------------------------------------------------------------------

    def is_out_struct(self, name):
        s = self.api.structs.get(name)
        return bool(s and (s.get('extensible') == 'out' or s.get('chained') == 'out' or s.get('out')))

    def chain_members(self, name):
        """The chained structures that may extend structure "name"."""
        result = []
        for k, v in self.api.structs.items():
            if v.get('chained') and name in v.get('chain roots', []):
                result.append(k)
        return result

    # ---- client serialisation -----------------------------------------------------------

    def ser_value(self, out, ind, type_name, expr, member=None):
        cat = self.api.category(type_name)
        p = ' ' * ind
        if cat == 'native':
            if type_name in NATIVE_32:
                out.append(f'{p}gexgpu_w_u32(w, (uint32_t)({expr}));')
            elif type_name in NATIVE_64:
                out.append(f'{p}gexgpu_w_u64(w, (uint64_t)({expr}));')
            elif type_name == 'float':
                out.append(f'{p}gexgpu_w_f32(w, {expr});')
            elif type_name == 'double':
                out.append(f'{p}gexgpu_w_f64(w, {expr});')
            elif type_name in ('void *', 'void const *'):
                out.append(f'{p}/* {type_name} {expr}: not carried. */')
            else:
                raise Exception(f'unhandled native type {type_name}')
        elif cat == 'enum':
            out.append(f'{p}gexgpu_w_u32(w, (uint32_t)({expr}));')
        elif cat == 'bitmask':
            out.append(f'{p}gexgpu_w_u64(w, (uint64_t)({expr}));')
        elif cat == 'object':
            out.append(f'{p}gexgpu_w_object(w, {expr});')
        elif cat == 'structure':
            if type_name == 'string view':
                out.append(f'{p}gexgpu_w_string(w, {expr});')
            elif type_name == 'future':
                out.append(f'{p}gexgpu_w_u64(w, ({expr}).id);')
            else:
                self.ser_needed.add(type_name)
                out.append(f'{p}ser_{c_type(type_name)}(w, &({expr}));')
        elif cat == 'callback info':
            raise Exception(f'callback info {type_name} in generic code')
        else:
            raise Exception(f'unhandled category {cat} for {type_name}')

    def ser_member(self, out, ind, m, base, length_expr):
        """Serialise member or argument "m" whose value is "base"."""
        p = ' ' * ind
        ann = m.get('annotation', 'value')
        t = m['type']
        if ann == 'value':
            self.ser_value(out, ind, t, base, m)
            return
        if ann == 'const*':
            if t in ('void', 'uint8_t', 'char') or self.api.category(t) == 'native' and t in ('void',):
                out.append(f'{p}gexgpu_w_blob(w, {base}, {base} ? (size_t)({length_expr}) : 0);')
                return
            if m.get('length') is not None:
                out.append(f'{p}{{')
                out.append(f'{p}    size_t i, n = {base} ? (size_t)({length_expr}) : 0;')
                out.append(f'{p}    gexgpu_w_u32(w, (uint32_t)n);')
                out.append(f'{p}    for (i = 0; i < n; ++i)')
                out.append(f'{p}    {{')
                self.ser_value(out, ind + 8, t, f'{base}[i]')
                out.append(f'{p}    }}')
                out.append(f'{p}}}')
                return
            out.append(f'{p}gexgpu_w_u32(w, {base} != NULL);')
            out.append(f'{p}if ({base})')
            out.append(f'{p}{{')
            self.ser_value(out, ind + 4, t, f'(*{base})')
            out.append(f'{p}}}')
            return
        raise Exception(f'unhandled annotation {ann} for {m}')

    def length_expr(self, m, prefix):
        length = m.get('length')
        if length is None:
            return '1'
        if isinstance(length, int):
            return str(length)
        if length == 'strlen':
            return f'strlen({prefix}{lower_camel(m["name"])})'
        return prefix + lower_camel(length)

    def gen_ser_struct(self, name, out):
        s = self.api.structs[name]
        ct = c_type(name)
        out.append(f'static void ser_{ct}(struct gexgpu_writer *w, const {ct} *s)')
        out.append('{')
        if s.get('extensible') == 'in':
            out.append('    ser_chain(w, s->nextInChain);')
        for m in s['members']:
            if self.api.category(m['type']) == 'callback info':
                out.append(f'    /* {m["name"]}: carried by hand. */')
                continue
            if m['type'] in ('void *',):
                continue
            self.ser_member(out, 4, m, f's->{member_c_name(m)}', self.length_expr(m, 's->'))
        out.append('}')
        out.append('')

    def gen_ser_chain(self, out):
        """The client's chain serialiser: every upstream chained struct that
        extends an input struct, by sType; others are dropped."""
        out.append('static void ser_chain(struct gexgpu_writer *w, const WGPUChainedStruct *chain)')
        out.append('{')
        out.append('    for (; chain; chain = chain->next)')
        out.append('    {')
        out.append('        switch (chain->sType)')
        out.append('        {')
        for k, v in sorted(self.api.structs.items()):
            if v.get('chained') != 'in' or k not in self.api.stypes:
                continue
            ct = c_type(k)
            out.append(f'            case WGPUSType_{camel(k)}:')
            out.append(f'                gexgpu_w_u32(w, chain->sType);')
            out.append(f'                ser_{ct}(w, (const {ct} *)chain);')
            out.append(f'                break;')
            self.ser_needed.add(k)
        out.append('            default:')
        out.append('                gexgpu_client_unsupported_chain(chain->sType);')
        out.append('                break;')
        out.append('        }')
        out.append('    }')
        out.append('    gexgpu_w_u32(w, 0);')
        out.append('}')
        out.append('')

    # ---- server deserialisation ---------------------------------------------------------

    def deser_value(self, out, ind, type_name, target):
        cat = self.api.category(type_name)
        p = ' ' * ind
        if cat == 'native':
            if type_name in NATIVE_32:
                out.append(f'{p}{target} = gexgpu_r_u32(r);')
            elif type_name in NATIVE_64:
                out.append(f'{p}{target} = gexgpu_r_u64(r);')
            elif type_name == 'float':
                out.append(f'{p}{target} = gexgpu_r_f32(r);')
            elif type_name == 'double':
                out.append(f'{p}{target} = gexgpu_r_f64(r);')
            elif type_name in ('void *', 'void const *'):
                out.append(f'{p}{target} = NULL;')
            else:
                raise Exception(f'unhandled native type {type_name}')
        elif cat == 'enum':
            out.append(f'{p}{target} = ({c_type(type_name)})gexgpu_r_u32(r);')
        elif cat == 'bitmask':
            out.append(f'{p}{target} = ({c_type(type_name)})gexgpu_r_u64(r);')
        elif cat == 'object':
            out.append(f'{p}{target} = ({c_type(type_name)})gexgpu_r_object(r);')
        elif cat == 'structure':
            if type_name == 'string view':
                out.append(f'{p}{target} = gexgpu_r_string(r);')
            elif type_name == 'future':
                out.append(f'{p}({target}).id = gexgpu_r_u64(r);')
            else:
                out.append(f'{p}deser_{c_type(type_name)}(r, &({target}));')
        else:
            raise Exception(f'unhandled category {cat} for {type_name}')

    def deser_member(self, out, ind, m, target, length_target):
        p = ' ' * ind
        ann = m.get('annotation', 'value')
        t = m['type']
        if ann == 'value':
            self.deser_value(out, ind, t, target)
            return
        if ann == 'const*':
            if t in ('void', 'uint8_t', 'char'):
                out.append(f'{p}{{')
                out.append(f'{p}    size_t n;')
                out.append(f'{p}    {target} = gexgpu_r_blob(r, &n);')
                if length_target:
                    out.append(f'{p}    {length_target} = n;')
                out.append(f'{p}}}')
                return
            ct = native_c(t) if self.api.category(t) == 'native' else c_type(t)
            if m.get('length') is not None:
                out.append(f'{p}{{')
                out.append(f'{p}    uint32_t i, n = gexgpu_r_u32(r);')
                out.append(f'{p}    {ct} *a = gexgpu_r_alloc(r, n * sizeof(*a));')
                out.append(f'{p}    for (i = 0; a && i < n; ++i)')
                out.append(f'{p}    {{')
                self.deser_value(out, ind + 8, t, 'a[i]')
                out.append(f'{p}    }}')
                out.append(f'{p}    {target} = n ? a : NULL;')
                if length_target:
                    out.append(f'{p}    {length_target} = n;')
                out.append(f'{p}}}')
                return
            out.append(f'{p}if (gexgpu_r_u32(r))')
            out.append(f'{p}{{')
            out.append(f'{p}    {ct} *a = gexgpu_r_alloc(r, sizeof(*a));')
            out.append(f'{p}    if (a)')
            out.append(f'{p}    {{')
            self.deser_value(out, ind + 8, t, '(*a)')
            out.append(f'{p}    }}')
            out.append(f'{p}    {target} = a;')
            out.append(f'{p}}}')
            out.append(f'{p}else')
            out.append(f'{p}{{')
            out.append(f'{p}    {target} = NULL;')
            out.append(f'{p}}}')
            return
        raise Exception(f'unhandled annotation {ann} for {m}')

    def gen_deser_struct(self, name, out):
        s = self.api.structs[name]
        ct = c_type(name)
        out.append(f'static void deser_{ct}(struct gexgpu_reader *r, {ct} *s)')
        out.append('{')
        out.append('    memset(s, 0, sizeof(*s));')
        if s.get('chained'):
            out.append(f'    s->chain.sType = WGPUSType_{camel(name)};')
        if s.get('extensible') == 'in':
            out.append('    s->nextInChain = deser_chain(r);')
        members = s['members']
        for m in members:
            if self.api.category(m['type']) == 'callback info' or m['type'] == 'void *':
                continue
            length = m.get('length')
            length_target = f's->{lower_camel(length)}' if isinstance(length, str) and length != 'strlen' else None
            self.deser_member(out, 4, m, f's->{member_c_name(m)}', length_target)
        out.append('}')
        out.append('')

    def gen_deser_chain(self, out):
        out.append('static WGPUChainedStruct *deser_chain(struct gexgpu_reader *r)')
        out.append('{')
        out.append('    WGPUChainedStruct *head = NULL, **tail = &head;')
        out.append('    uint32_t stype;')
        out.append('')
        out.append('    while (!r->error && (stype = gexgpu_r_u32(r)))')
        out.append('    {')
        out.append('        WGPUChainedStruct *link = NULL;')
        out.append('')
        out.append('        switch (stype)')
        out.append('        {')
        for k, v in sorted(self.api.structs.items()):
            if v.get('chained') != 'in' or k not in self.api.stypes:
                continue
            ct = c_type(k)
            out.append(f'            case WGPUSType_{camel(k)}:')
            out.append(f'            {{')
            out.append(f'                {ct} *s = gexgpu_r_alloc(r, sizeof(*s));')
            out.append(f'                if (s)')
            out.append(f'                    deser_{ct}(r, s);')
            out.append(f'                link = (WGPUChainedStruct *)s;')
            out.append(f'                break;')
            out.append(f'            }}')
        out.append('            default:')
        out.append('                r->error = true;')
        out.append('                break;')
        out.append('        }')
        out.append('        if (!link)')
        out.append('            break;')
        out.append('        *tail = link;')
        out.append('        tail = &link->next;')
        out.append('    }')
        out.append('    return head;')
        out.append('}')
        out.append('')

    # ---- methods ------------------------------------------------------------------------

    def method_c_name(self, obj, m):
        return 'wgpu' + camel(obj) + camel(m['name'])

    def args_c(self, obj, m):
        args = []
        if obj:
            args.append(f'{c_type(obj)} self')
        for a in m.get('args', []):
            args.append(self.arg_decl(a))
        return ', '.join(args) if args else 'void'

    def arg_decl(self, a):
        t = a['type']
        cat = self.api.category(t)
        ann = a.get('annotation', 'value')
        if cat == 'native':
            base = {'bool': 'WGPUBool', 'void': 'void', 'void *': 'void *', 'void const *': 'void const *'}.get(t, t)
        else:
            base = c_type(t)
        if ann == 'value':
            return f'{base} {lower_camel(a["name"])}'
        if ann == 'const*':
            return f'{base} const * {lower_camel(a["name"])}'
        if ann == '*':
            return f'{base} * {lower_camel(a["name"])}'
        raise Exception(f'unhandled arg annotation {ann}')

    def returns_c(self, m):
        r = ret_type(m)
        if not r:
            return 'void'
        if self.api.category(r) == 'native':
            return {'bool': 'WGPUBool', 'void *': 'void *', 'void const *': 'void const *'}.get(r, r)
        return c_type(r)

    def classify(self, obj, m):
        """'void', 'object' or 'sync' for generated methods."""
        r = ret_type(m)
        has_out = any(a.get('annotation') == '*' for a in m.get('args', []))
        if any(self.api.category(a['type']) == 'callback info' for a in m.get('args', [])):
            raise Exception(f'callback in generated method {obj} {m["name"]}')
        if r and self.api.category(r) == 'object' and not has_out:
            return 'object'
        if not r and not has_out:
            return 'void'
        return 'sync'

    def collect(self):
        for obj in sorted(self.api.objects):
            for m in self.api.methods(obj):
                cname = self.method_c_name(obj, m)
                if m['name'] in ('add ref', 'release'):
                    continue
                self.ops.append((f'GEXGPU_OP_{camel(obj)}{camel(m["name"])}', cname, obj, m))
        for name, f in sorted(self.api.functions()):
            cname = 'wgpu' + camel(name)
            self.ops.append((f'GEXGPU_OP_{camel(name)}', cname, None, f))

    def gen_client_method(self, op, cname, obj, m, out):
        kind = self.classify(obj, m)
        ret = self.returns_c(m)
        out.append(f'WGPU_EXPORT {ret} GEXGPU_CLIENT_NAME({cname})({self.args_c(obj, m)})')
        out.append('{')
        out.append('    struct gexgpu_writer *w;')
        if kind == 'object':
            out.append(f'    struct gexgpu_object *result = gexgpu_client_new_object(GEXGPU_TYPE_{camel(ret_type(m))}, '
                       f'{"(struct gexgpu_object *)self" if obj else "NULL"});')
        if kind == 'sync':
            out.append('    struct gexgpu_reader *r;')
            if ret != 'void':
                out.append(f'    {ret} ret;')
        out.append('')
        out.append(f'    w = gexgpu_client_begin({"(struct gexgpu_object *)self" if obj else "NULL"}, {op});')
        if obj:
            out.append('    gexgpu_w_object(w, self);')
        for a in m.get('args', []):
            ann = a.get('annotation', 'value')
            if ann == '*':
                if self.is_out_struct(a['type']):
                    # Chained extensions of out structs are not carried.
                    out.append(f'    gexgpu_w_u32(w, 0);')
                    self.out_needed.add(a['type'])
                    continue
                raise Exception(f'out arg of non-out type {a}')
            length = a.get('length')
            length_expr = lower_camel(length) if isinstance(length, str) else (str(length) if length else '1')
            self.ser_member(out, 4, a, lower_camel(a['name']), length_expr)
        if kind == 'object':
            out.append('    gexgpu_w_u32(w, result->id);')
        if kind == 'object':
            out.append('    gexgpu_client_end(w);')
            out.append(f'    return ({ret})result;')
        elif kind == 'void':
            out.append('    gexgpu_client_end(w);')
        elif kind == 'sync':
            out.append('    r = gexgpu_client_end_call(w);')
            if ret != 'void':
                cat = self.api.category(ret_type(m))
                if cat == 'bitmask' or ret_type(m) in NATIVE_64:
                    out.append(f'    ret = ({ret})gexgpu_r_u64(r);')
                else:
                    out.append(f'    ret = ({ret})gexgpu_r_u32(r);')
            for a in m.get('args', []):
                if a.get('annotation') == '*':
                    out.append(f'    out_{c_type(a["type"])}(r, {lower_camel(a["name"])});')
            out.append('    gexgpu_client_call_done(r);')
            if ret != 'void':
                out.append('    return ret;')
        out.append('}')
        out.append('')

    def gen_server_case(self, op, cname, obj, m, out):
        kind = self.classify(obj, m)
        proc = cname[4].lower() + cname[5:]
        out.append(f'        case {op}:')
        out.append('        {')
        decls, call_args = [], []
        if obj:
            decls.append(f'{c_type(obj)} self = ({c_type(obj)})gexgpu_r_object(r);')
            call_args.append('self')
        body = []
        for a in m.get('args', []):
            ann = a.get('annotation', 'value')
            name = lower_camel(a['name'])
            if ann == '*':
                st = c_type(a['type'])
                decls.append(f'{st} *{name} = gexgpu_r_alloc(r, sizeof(*{name}));')
                body.append(f'if ({name}) deser_out_chain_{st}(r, {name});')
                call_args.append(name)
                continue
            if ann == 'value':
                decls.append(f'{self.arg_decl(a)};')
            else:
                t = a['type']
                if t in ('void', 'uint8_t', 'char'):
                    decls.append(f'void const *{name};')
                else:
                    ct = native_c(t) if self.api.category(t) == 'native' else c_type(t)
                    decls.append(f'{ct} const *{name};')
            call_args.append(name)
            length = a.get('length')
            # An argument array's length is another argument, read after it.
            sub = []
            self.deser_member(sub, 0, a, name, None)
            body.extend(sub)
        for d in decls:
            out.append(f'            {d}')
        if kind == 'object':
            out.append('            uint32_t result_id;')
        for b in body:
            out.append(f'            {b}')
        if kind == 'object':
            out.append('            result_id = gexgpu_r_u32(r);')
        out.append('            if (r->error)')
        out.append('                return false;')
        if obj:
            # A null receiver (a client's object that was never made) would
            # crash the webgpu.h below: the call is skipped, and the batch
            # goes on. One with a reply cannot be skipped quietly.
            out.append('            if (!self)')
            out.append('                ' + ('break;' if kind in ('object', 'void') else 'return false;'))
        call = f'server->procs.{proc}({", ".join(call_args)})'
        if kind == 'object':
            out.append(f'            gexgpu_server_set_object(server, result_id, GEXGPU_TYPE_{camel(ret_type(m))}, {call});')
        elif kind == 'void':
            out.append(f'            {call};')
        else:
            ret = ret_type(m)
            if ret:
                cat = self.api.category(ret)
                if cat == 'bitmask' or ret in NATIVE_64:
                    out.append(f'            gexgpu_w_u64(&server->reply, (uint64_t){call});')
                else:
                    out.append(f'            gexgpu_w_u32(&server->reply, (uint32_t){call});')
            else:
                out.append(f'            {call};')
            for a in m.get('args', []):
                if a.get('annotation') == '*':
                    out.append(f'            ser_out_{c_type(a["type"])}(&server->reply, {lower_camel(a["name"])});')
            out.append('            gexgpu_server_reply_done(server);')
        out.append('            break;')
        out.append('        }')

    # ---- out structs (server -> client) -------------------------------------------------

    def gen_out_structs(self, client, server):
        for name in sorted(self.out_needed):
            s = self.api.structs[name]
            ct = c_type(name)
            # Server: the chain to fill, then the struct's members back.
            server.append(f'static void deser_out_chain_{ct}(struct gexgpu_reader *r, {ct} *s)')
            server.append('{')
            server.append('    memset(s, 0, sizeof(*s));')
            server.append('    gexgpu_r_u32(r); /* No chained extensions. */')
            server.append('}')
            server.append('')
            server.append(f'static void ser_out_{ct}(struct gexgpu_writer *w, const {ct} *s)')
            server.append('{')
            client.append(f'static void out_{ct}(struct gexgpu_reader *r, {ct} *s)')
            client.append('{')
            for m in s['members']:
                n = member_c_name(m)
                t = m['type']
                ann = m.get('annotation', 'value')
                if ann == 'value':
                    sv, cv = [], []
                    self.ser_value_plain(sv, t, f's->{n}')
                    server.extend('    ' + x for x in sv)
                    self.deser_value(cv, 4, t, f's->{n}')
                    client.extend(cv)
                elif ann == 'const*' and m.get('length') is not None:
                    length = lower_camel(m['length'])
                    server.append(f'    {{')
                    server.append(f'        size_t i;')
                    server.append(f'        gexgpu_w_u32(w, (uint32_t)s->{length});')
                    server.append(f'        for (i = 0; i < s->{length}; ++i)')
                    server.append(f'        {{')
                    sv = []
                    self.ser_value_plain(sv, t, f's->{n}[i]')
                    server.extend('            ' + x for x in sv)
                    server.append(f'        }}')
                    server.append(f'    }}')
                    et = c_type(t)
                    client.append(f'    {{')
                    client.append(f'        uint32_t i, n = gexgpu_r_u32(r);')
                    client.append(f'        {et} *a = gexgpu_client_alloc_out(n * sizeof(*a));')
                    client.append(f'        for (i = 0; a && i < n; ++i)')
                    client.append(f'        {{')
                    cv = []
                    self.deser_value(cv, 12, t, 'a[i]')
                    client.extend(cv)
                    client.append(f'        }}')
                    client.append(f'        s->{n} = a;')
                    client.append(f'        s->{length} = n;')
                    client.append(f'    }}')
                else:
                    raise Exception(f'unhandled out member {name} {m}')
            server.append('}')
            server.append('')
            client.append('}')
            client.append('')

    def ser_value_plain(self, out, t, expr):
        cat = self.api.category(t)
        if cat == 'structure' and t == 'string view':
            out.append(f'gexgpu_w_string(w, {expr});')
        elif cat == 'structure':
            raise Exception(f'nested out struct {t}')
        else:
            tmp = []
            self.ser_value(tmp, 0, t, expr)
            out.extend(tmp)

    # ---- output -------------------------------------------------------------------------

    def run(self):
        self.collect()
        header, client, server, rename = [], [], [], []
        banner = '/* Generated by tools/gen_webgpu_wire.py from third_party/dawn-api/dawn.json; do not edit. */'

        # Methods first: they decide which structs are serialised.
        client_methods, server_cases = [], []
        for op, cname, obj, m in self.ops:
            if cname not in HANDWRITTEN_CLIENT:
                self.gen_client_method(op, cname, obj, m, client_methods)
            if cname not in HANDWRITTEN_SERVER and cname not in CLIENT_ONLY:
                self.gen_server_case(op, cname, obj, m, server_cases)
        # Reference counts: the client counts, and tells the server when an
        # object goes.
        for obj in sorted(self.api.objects):
            ct = c_type(obj)
            client_methods.append(f'WGPU_EXPORT void GEXGPU_CLIENT_NAME(wgpu{camel(obj)}AddRef)({ct} self)')
            client_methods.append('{')
            client_methods.append('    gexgpu_client_addref((struct gexgpu_object *)self);')
            client_methods.append('}')
            client_methods.append('')
            client_methods.append(f'WGPU_EXPORT void GEXGPU_CLIENT_NAME(wgpu{camel(obj)}Release)({ct} self)')
            client_methods.append('{')
            client_methods.append('    gexgpu_client_release((struct gexgpu_object *)self);')
            client_methods.append('}')
            client_methods.append('')
        # Structs the handwritten calls carry.
        for s in ('buffer descriptor', 'texture descriptor', 'query set descriptor', 'request adapter options',
                  'device descriptor', 'compute pipeline descriptor', 'render pipeline descriptor',
                  'surface configuration', 'queue descriptor', 'limits'):
            if s in self.api.structs:
                self.ser_needed.add(s)
        self.out_needed.update({'limits', 'adapter info', 'supported features'})

        ser_bodies = []
        self.gen_ser_chain(ser_bodies)
        done = set()
        while self.ser_needed - done:
            for name in sorted(self.ser_needed - done):
                body = []
                self.gen_ser_struct(name, body)
                ser_bodies.extend(body)
                done.add(name)
        deser_bodies = []
        self.gen_deser_chain(deser_bodies)
        for name in sorted(done):
            self.gen_deser_struct(name, deser_bodies)
        out_client, out_server = [], []
        self.gen_out_structs(out_client, out_server)

        # Header.
        header += [banner, '', '#ifndef GEXGPU_WIRE_GEN_H', '#define GEXGPU_WIRE_GEN_H', '',
                   'enum gexgpu_op', '{', '    GEXGPU_OP_INVALID = 0,']
        for op, cname, obj, m in self.ops:
            header.append(f'    {op},')
        header += ['    GEXGPU_OP_RELEASE,', '    GEXGPU_OP_BUFFER_UPDATE,', '    GEXGPU_OP_COUNT,', '};', '',
                   'enum gexgpu_type', '{', '    GEXGPU_TYPE_NONE = 0,']
        for obj in sorted(self.api.objects):
            header.append(f'    GEXGPU_TYPE_{camel(obj)},')
        header += ['    GEXGPU_TYPE_COUNT,', '};', '']
        header += ['/* The server\'s webgpu.h, by function pointer: a real implementation, which a', ' * process with the client in it cannot call by name. */',
                   'struct gexgpu_procs', '{']
        for obj in sorted(self.api.objects):
            for m in self.api.methods(obj):
                cname = self.method_c_name(obj, m)
                header.append(f'    WGPUProc{cname[4:]} {cname[4].lower() + cname[5:]};')
        for name, f in sorted(self.api.functions()):
            cname = 'wgpu' + camel(name)
            header.append(f'    WGPUProc{cname[4:]} {cname[4].lower() + cname[5:]};')
        for obj in sorted(self.api.objects):
            header.append(f'    WGPUProc{camel(obj)}AddRef {lower_camel(obj)}AddRef;')
            header.append(f'    WGPUProc{camel(obj)}Release {lower_camel(obj)}Release;')
        header += ['};', '',
                   '/* Fills "procs" with the webgpu.h this file is compiled against. */',
                   '#define GEXGPU_PROCS_INIT_DIRECT(procs) \\', '    do { \\']
        for obj in sorted(self.api.objects):
            for m in self.api.methods(obj):
                cname = self.method_c_name(obj, m)
                header.append(f'        (procs)->{cname[4].lower() + cname[5:]} = {cname}; \\')
        for name, f in sorted(self.api.functions()):
            cname = 'wgpu' + camel(name)
            header.append(f'        (procs)->{cname[4].lower() + cname[5:]} = {cname}; \\')
        for obj in sorted(self.api.objects):
            header.append(f'        (procs)->{lower_camel(obj)}AddRef = wgpu{camel(obj)}AddRef; \\')
            header.append(f'        (procs)->{lower_camel(obj)}Release = wgpu{camel(obj)}Release; \\')
        header += ['    } while (0)', '', '#endif', '']

        client = [banner, '', 'static void ser_chain(struct gexgpu_writer *w, const WGPUChainedStruct *chain);'] \
            + [f'static void ser_{c_type(n)}(struct gexgpu_writer *w, const {c_type(n)} *s);' for n in sorted(done)] \
            + [''] + ser_bodies + out_client + client_methods
        server = [banner, '', 'static WGPUChainedStruct *deser_chain(struct gexgpu_reader *r);', ''] \
            + [f'static void deser_{c_type(n)}(struct gexgpu_reader *r, {c_type(n)} *s);' for n in sorted(done)] \
            + [''] + deser_bodies + out_server
        server += ['static void gexgpu_server_release_object(struct gexgpu_server *server, uint32_t type, void *object)',
                   '{', '    switch (type)', '    {']
        for obj in sorted(self.api.objects):
            server += [f'        case GEXGPU_TYPE_{camel(obj)}:',
                       f'            server->procs.{lower_camel(obj)}Release(({c_type(obj)})object);',
                       '            break;']
        server += ['        default:', '            break;', '    }', '}', '']
        server += ['static bool gexgpu_server_dispatch_gen(struct gexgpu_server *server, uint32_t op,',
                   '        struct gexgpu_reader *r)', '{', '    switch (op)', '    {'] + server_cases + \
                  ['        default:', '            return gexgpu_server_dispatch_handwritten(server, op, r);', '    }',
                   '    return true;', '}', '']

        rename += [banner, '', '#ifndef GEXGPU_WIRE_RENAME_GEN_H', '#define GEXGPU_WIRE_RENAME_GEN_H', '']
        names = set()
        for obj in sorted(self.api.objects):
            for m in self.api.methods(obj):
                names.add(self.method_c_name(obj, m))
        for name, f in self.api.functions():
            names.add('wgpu' + camel(name))
        names.update(n for n in HANDWRITTEN_CLIENT)
        for obj in self.api.objects:
            names.add(f'wgpu{camel(obj)}AddRef')
            names.add(f'wgpu{camel(obj)}Release')
        for n in sorted(names):
            rename.append(f'#define {n} gex_{n}')
        rename += ['', '#endif', '']

        os.makedirs(OUT, exist_ok=True)
        for fname, lines in (('wire-gen.h', header), ('wire-client-gen.inc', client),
                             ('wire-server-gen.inc', server), ('wire-rename-gen.h', rename)):
            with open(os.path.join(OUT, fname), 'w') as f:
                f.write('\n'.join(lines))
        print(f'{len(self.ops)} ops, {len(done)} structs serialised, {len(self.out_needed)} out structs')


if __name__ == '__main__':
    Gen(Api(API)).run()
