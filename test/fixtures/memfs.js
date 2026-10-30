// A mount provider over a Map, for the tests: paths inside the mount, an
// optional delay on each call (a slow provider, such as a network one).
const err = (code) => Object.assign(new Error(code), { code });
const parent = (p) => p.replace(/\/[^/]*$/, '') || '/';

export class MemFs {
  constructor(files = {}, { delay = 0 } = {}) {
    this.nodes = new Map([['/', { type: 'directory', mode: 0o755, mtime: Date.now() }]]);
    this.delay = delay;
    this.calls = [];
    this.handles = new Map();
    this.next = 1;
    for (const [p, text] of Object.entries(files)) this.put(p, text);
  }
  put(p, data) {
    const parts = p.split('/').filter(Boolean);
    for (let i = 1; i < parts.length; i++) {
      const d = `/${parts.slice(0, i).join('/')}`;
      if (!this.nodes.has(d)) this.nodes.set(d, { type: 'directory', mode: 0o755, mtime: Date.now() });
    }
    const bytes = typeof data === 'string' ? new TextEncoder().encode(data) : data;
    this.nodes.set(p, { type: 'file', mode: 0o644, mtime: Date.now(), data: bytes });
  }
  text(p) {
    const n = this.nodes.get(p);
    return n?.data ? new TextDecoder().decode(n.data) : null;
  }
  async wait(op, p) {
    this.calls.push(`${op} ${p}`);
    if (this.delay) await new Promise((r) => setTimeout(r, this.delay));
  }
  node(p) {
    const n = this.nodes.get(p);
    if (!n) throw err('ENOENT');
    return n;
  }
  async stat(p) {
    await this.wait('stat', p);
    const n = this.node(p);
    return { type: n.type, mode: n.mode, size: n.data?.length ?? 0, mtime: n.mtime };
  }
  async readdir(p) {
    await this.wait('readdir', p);
    if (this.node(p).type !== 'directory') throw err('ENOTDIR');
    const pre = p === '/' ? '/' : `${p}/`;
    return [...this.nodes.keys()].filter((k) => k !== p && k.startsWith(pre) && !k.slice(pre.length).includes('/'))
      .map((k) => ({ name: k.slice(pre.length), type: this.nodes.get(k).type }));
  }
  async open(p, o) {
    await this.wait('open', p);
    if (!this.nodes.has(p)) {
      if (this.node(parent(p)).type !== 'directory') throw err('ENOTDIR');
      this.nodes.set(p, { type: 'file', mode: o.mode || 0o644, mtime: Date.now(), data: new Uint8Array(0) });
    } else if (o.truncate) this.nodes.get(p).data = new Uint8Array(0);
    const h = this.next++;
    this.handles.set(h, p);
    return h;
  }
  async read(h, off, len) {
    await this.wait('read', this.handles.get(h));
    return this.node(this.handles.get(h)).data.slice(off, off + len);
  }
  async write(h, off, bytes) {
    await this.wait('write', this.handles.get(h));
    const n = this.node(this.handles.get(h));
    const end = Math.max(n.data.length, off + bytes.length);
    const d = new Uint8Array(end);
    d.set(n.data);
    d.set(bytes, off);
    n.data = d;
    n.mtime = Date.now();
    return bytes.length;
  }
  async close(h) {
    this.handles.delete(h);
  }
  async truncate(p, len) {
    const n = this.node(p);
    const d = new Uint8Array(len);
    d.set(n.data.subarray(0, len));
    n.data = d;
  }
  async mkdir(p, mode) {
    await this.wait('mkdir', p);
    if (this.nodes.has(p)) throw err('EEXIST');
    this.node(parent(p));
    this.nodes.set(p, { type: 'directory', mode, mtime: Date.now() });
  }
  async unlink(p) {
    await this.wait('unlink', p);
    if (this.node(p).type === 'directory') throw err('EISDIR');
    this.nodes.delete(p);
  }
  async rmdir(p) {
    await this.wait('rmdir', p);
    if (this.node(p).type !== 'directory') throw err('ENOTDIR');
    if ([...this.nodes.keys()].some((k) => k.startsWith(`${p}/`))) throw err('ENOTEMPTY');
    this.nodes.delete(p);
  }
  async rename(a, b) {
    await this.wait('rename', a);
    this.node(a);
    for (const k of [...this.nodes.keys()]) {
      if (k === a || k.startsWith(`${a}/`)) {
        const n = this.nodes.get(k);
        this.nodes.delete(k);
        this.nodes.set(b + k.slice(a.length), n);
      }
    }
  }
  async chmod(p, mode) {
    this.node(p).mode = mode;
  }
}
