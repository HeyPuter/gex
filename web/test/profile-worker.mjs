// CDP CPU profiler: name the hottest wasm function in each guest Worker.
//
// The decisive tool for a CPU-pegged browser hang that no guest-level probe can
// see: a wasm spin with no gex safepoint (the allocator, a shim loop) is
// invisible to ?heartbeat/?enterlog, but a Chrome DevTools CPU profile names
// the exact function. It cracked the winemine wedge -- 100% self-time in
// <Counting as GlobalAlloc>::alloc (the shared talc heap spinlock), called from
// gex_dump_threads (the OOB-trap crash reporter). See winemine-browser memory.
//
// Usage: launch a run (drive.mjs) so a guest Worker is busy, then, while it is:
//   node web/test/profile-worker.mjs        # profiles every worker target on :9333
// Prints, per busy worker, the hottest node and its leaf->root stack.
const BASE='http://localhost:9333';
const sleep=ms=>new Promise(r=>setTimeout(r,ms));
const wt=(p,ms)=>Promise.race([p,sleep(ms).then(()=>{throw new Error('t/o')})]);
async function targets(){return await (await fetch(`${BASE}/json`)).json();}
function rpc(ws){let id=0;const pend=new Map();ws.addEventListener('message',ev=>{const m=JSON.parse(ev.data);if(m.id&&pend.has(m.id)){pend.get(m.id)(m.result||{});pend.delete(m.id);}});return (me,pa={})=>wt(new Promise(res=>{const i=++id;pend.set(i,res);ws.send(JSON.stringify({id:i,method:me,params:pa}));}),8000);}
async function prof(t,secs){const ws=new WebSocket(t.webSocketDebuggerUrl);await wt(new Promise((res,rej)=>{ws.addEventListener('open',res);ws.addEventListener('error',rej);}),3000);const call=rpc(ws);await call('Profiler.enable');await call('Profiler.setSamplingInterval',{interval:120});await call('Profiler.start');await sleep(secs*1000);const {profile:p}=await call('Profiler.stop');ws.close();return p;}
function analyze(p){
  const self=new Map();for(const s of (p.samples||[]))self.set(s,(self.get(s)||0)+1);
  const byId=new Map(p.nodes.map(n=>[n.id,n]));
  const parent=new Map();for(const n of p.nodes)for(const c of (n.children||[]))parent.set(c,n.id);
  const nodes=p.nodes.map(n=>({id:n.id,fn:n.callFrame.functionName||'(anon)',url:n.callFrame.url||'',self:self.get(n.id)||0})).sort((a,b)=>b.self-a.self);
  const hot=nodes[0];
  // ancestor chain from hot up to root
  const chain=[]; let cur=hot.id; for(let i=0;i<20 && cur!=null;i++){const n=byId.get(cur);chain.push(`${n.callFrame.functionName||'(anon)'} @ ${(n.callFrame.url||'').slice(-24)}`);cur=parent.get(cur);}
  return {total:(p.samples||[]).length, hot, chain, top:nodes.slice(0,6)};
}
(async()=>{
  const ts=(await targets()).filter(t=>t.type==='worker'&&t.webSocketDebuggerUrl);
  const res=await Promise.allSettled(ts.map(async t=>({t,p:await prof(t,4)})));
  for(const x of res){ if(x.status!=='fulfilled')continue; const {t,p}=x.value; const a=analyze(p);
    if(a.total<40||a.hot.fn==='(idle)'||a.hot.fn==='(program)')continue;
    console.log(`\n=== ${(t.url||'').slice(-26)} samples=${a.total} hot=${a.hot.fn} (${a.hot.self}) ===`);
    console.log('  STACK (leaf->root):'); a.chain.forEach(c=>console.log('    '+c));
  }
  console.log('done');
})().catch(e=>console.log('FATAL',e.message));
