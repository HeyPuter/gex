# Main-Worker profile buckets for HK (JIT vs interpreter). usage: hkbuckets.py SHOTDIR...
# managed = inclusive time under gexp_mono_runtime_invoke; frame normaliser = UnityPlayer (f_*) self time outside it.
import json,collections,glob,sys,re
def bucket(n):
    if n=='major_get_used_size': return 'major_get_used_size'
    if n.startswith('f_'): return 'UnityPlayer code'
    if n.startswith('mono_interp_') and 'exec' in n: return 'interp exec'
    if re.match(r'^[^ ]+ .*:.*\(',n) or n.startswith('('): return 'JIT code'
    if n.startswith(('interp_entry','wj_','mono_wasm_jit','mono_interp_get_imethod','get_imethod','gex_interp')): return 'jit/interp glue'
    if n.startswith('mono_thread') or 'gc_safe' in n or 'gc_unsafe' in n: return 'GC-mode transitions'
    if n.startswith(('gexp_call_guest','provider_try','wasm-to-js','js-to-wasm','gexp_hook')): return 'guest-call glue'
    return 'other'
for dd in sys.argv[1:]:
    fs=[f for f in glob.glob(dd+'/worker-*.cpuprofile')]
    best=None
    for f in fs:
        p=json.load(open(f)); 
        if best is None or len(p['samples'])>len(best[1]['samples']): best=(f,p)
    f,p=best; nodes={n['id']:n for n in p['nodes']}; par={}
    for n in p['nodes']:
        for c in n.get('children',[]): par[c]=n['id']
    under={}
    def isunder(i):
        path=[]; r=False
        while i is not None:
            if i in under: r=under[i]; break
            path.append(i)
            if nodes[i]['callFrame']['functionName']=='gexp_mono_runtime_invoke': r=True; break
            i=par.get(i)
        for x in path: under[x]=r
        return r
    b=collections.Counter(); out_up=0; tot=0
    s=p['samples']; d=p['timeDeltas']
    for k,x in enumerate(s):
        t=(d[k+1] if k+1<len(d) else 0)/1e6; tot+=t
        n=nodes[x]['callFrame']['functionName']
        if isunder(x): b[bucket(n)]+=t; b['=managed total']+=t
        elif n.startswith('f_'): out_up+=t
    print(f'== {dd} ({f.split("/")[-1]}) window {tot:.2f}s, UnityPlayer outside managed {out_up:.2f}s')
    for k,v in sorted(b.items()): print(f'  {k:24} {v:6.2f}s  per UP-s {v/out_up if out_up else 0:5.3f}')
