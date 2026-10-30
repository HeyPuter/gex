// A read-only mount provider over files on the page's server: `list` is
// [[path, size, type]] (type 'file' or 'directory'), `base` the URL the
// paths are under. Reads are ranged requests.
const err = (code) => Object.assign(new Error(code), { code });

export class HttpFs {
  constructor(base, list) {
    this.base = base.replace(/\/$/, '');
    this.nodes = new Map([['/', { type: 'directory', size: 0 }]]);
    this.kids = new Map([['/', []]]);
    for (const [p, size, type] of list) {
      this.nodes.set(p, { type, size });
      if (type === 'directory') this.kids.set(p, this.kids.get(p) ?? []);
      const up = p.replace(/\/[^/]*$/, '') || '/';
      (this.kids.get(up) ?? this.kids.set(up, []).get(up)).push({ name: p.slice(up.length + (up === '/' ? 0 : 1)), type });
    }
  }
  node(p) {
    const n = this.nodes.get(p);
    if (!n) throw err('ENOENT');
    return n;
  }
  async stat(p) {
    const n = this.node(p);
    return { type: n.type, size: n.size, mode: n.type === 'directory' ? 0o755 : 0o644, mtime: 0 };
  }
  async readdir(p) {
    if (this.node(p).type !== 'directory') throw err('ENOTDIR');
    return this.kids.get(p) ?? [];
  }
  async open(p, o) {
    if (o.write || o.create) throw err('EROFS');
    if (this.node(p).type !== 'file') throw err('EISDIR');
    return p;
  }
  async read(p, off, len) {
    const size = this.node(p).size;
    if (off >= size) return new Uint8Array(0);
    const end = Math.min(size, off + len);
    const r = await fetch(`${this.base}${p}`, { headers: { Range: `bytes=${off}-${end - 1}` } });
    if (!r.ok) throw err('EIO');
    const b = new Uint8Array(await r.arrayBuffer());
    return r.status === 206 ? b : b.subarray(off, end);
  }
  async write() { throw err('EROFS'); }
  async close() {}
  async mkdir() { throw err('EROFS'); }
  async unlink() { throw err('EROFS'); }
  async rename() { throw err('EROFS'); }
}
