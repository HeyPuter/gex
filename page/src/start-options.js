// The checks app.js's `start()` makes of its options before it fetches
// anything: an unknown name (a typo would otherwise be ignored) or a value
// of the wrong kind is an error that names it. No DOM, so Node can test it
// (test/page/start-options).

/** Every option `start` reads (app.js lists what each is). */
export const OPTIONS = [
  'container', 'canvas', 'size', 'args', 'env', 'cwd', 'exe', 'uid', 'mtimes', 'filesystem', 'network', 'wisp',
  'pageHosts', 'strace', 'trapstack', 'debug', 'compositor', 'gl', 'translation', 'hostx', 'mem64',
  'glLazy', 'background', 'baseUrl', 'launch', 'build', 'onLog', 'onExit', 'onProgress', 'onStoreBusy', 'onCrash', 'ready',
];

/** The option of `names` one edit or a case away from `k`, if any. */
function nearName(k, names) {
  const low = k.toLowerCase();
  const near = (a, b) => {
    if (Math.abs(a.length - b.length) > 1) return false;
    let i = 0;
    while (i < a.length && a[i] === b[i]) i++;
    const rest = (x, y) => a.slice(i + x) === b.slice(i + y);
    return rest(1, 1) || rest(1, 0) || rest(0, 1) || (a[i] === b[i + 1] && a[i + 1] === b[i] && rest(2, 2));
  };
  return names.find((n) => n.toLowerCase() === low) ?? names.find((n) => near(low, n.toLowerCase())) ?? null;
}

/** A mistake in the options, named before anything is fetched: an unknown
 *  name (a typo would otherwise be ignored), or a value of the wrong kind. */
export function checkOptions(opts) {
  if (opts === null || typeof opts !== 'object') throw new Error('start(options): options is an object');
  const bad = (k, what) => {
    throw new Error(`start({ ${k}: ${showValue(opts[k])} }): ${what}`);
  };
  for (const k of Object.keys(opts)) {
    if (OPTIONS.includes(k)) continue;
    // The guest directory kept in IndexedDB: an OPFS filesystem keeps the
    // whole root now.
    if (k === 'persist') throw new Error("start({ persist }): gone: a filesystem kept across loads keeps the whole root, filesystem: { kind: 'opfs', name }");
    const n = nearName(k, OPTIONS);
    throw new Error(`start({ ${k} }): no such option${n ? `; did you mean ${n}?` : ''} (the options: ${OPTIONS.join(', ')})`);
  }
  const given = (k) => opts[k] !== undefined && opts[k] !== null;
  if (given('size') && !sizeValue(opts.size)) bad('size', "'fit', {w, h} or 'WxH', in pixels");
  if (given('args') && !(Array.isArray(opts.args) && opts.args.every((a) => typeof a === 'string'))) bad('args', 'an array of strings');
  if (given('env')) {
    const e = opts.env;
    const ok = Array.isArray(e) ? e.every((x) => typeof x === 'string' && /^[^=]+=/.test(x)) : typeof e === 'object';
    if (!ok) bad('env', "['NAME=value', ...] or {NAME: value}");
  }
  for (const k of ['cwd', 'exe', 'launch']) if (given(k) && typeof opts[k] !== 'string') bad(k, 'a string');
  if (given('uid') && !(Number.isInteger(opts.uid) && opts.uid >= 0)) bad('uid', 'a user id (a whole number)');
  if (given('strace') && typeof opts.strace !== 'boolean' && !Number.isInteger(opts.strace)) bad('strace', 'true (every process) or a pid');
  if (given('wisp') && !/^wss?:\/\//.test(String(opts.wisp))) bad('wisp', 'a Wisp proxy\'s URL, ws://... or wss://...');
  if (given('network') && typeof opts.network?.connect !== 'function') bad('network', 'an object with connect(host, port)');
  if (given('background')) {
    const bg = opts.background, colour = (c) => /^#[0-9a-fA-F]{6}$/.test(String(c));
    const ok = typeof bg === 'string' ? colour(bg)
      : bg && typeof bg === 'object' && (bg.color === undefined || colour(bg.color))
        && (bg.image === undefined || typeof bg.image === 'string' || bg.image instanceof Blob)
        && (bg.anchor === undefined || ['top', 'center', 'bottom'].includes(bg.anchor));
    if (!ok) bad('background', "a colour, '#rrggbb', or {color: '#rrggbb', image: URL or Blob, anchor: 'top'|'center'|'bottom'}");
  }
  if (given('hostx') && ![true, false, 'rootful'].includes(opts.hostx)) bad('hostx', "true, false or 'rootful'");
  if (given('filesystem') && !filesystemValue(opts.filesystem)) bad('filesystem', "'memory', { kind: 'memory', max }, or { kind: 'opfs', name, cache }");
  for (const k of ['onLog', 'onExit', 'onProgress', 'onStoreBusy', 'onCrash']) if (given(k) && typeof opts[k] !== 'function') bad(k, 'a function');
  for (const k of ['debug', 'pageHosts']) if (given(k) && typeof opts[k] !== 'object') bad(k, 'an object, {name: value}');
}

/** Where the guest's files are kept: `'memory'` (`MemoryFilesystem`),
 *  `{ kind: 'memory', max }`, or `{ kind: 'opfs', name, cache }`
 *  (`OPFSFilesystem`), as `{ kind, ... }`; null if it is not one. */
export function filesystemValue(f) {
  if (f === 'memory') return { kind: 'memory' };
  if (typeof f !== 'object' || f === null) return null;
  if (f.kind === 'memory') return (f.max === undefined || (Number.isSafeInteger(f.max) && f.max > 0)) ? f : null;
  if (f.kind === 'opfs') {
    return typeof f.name === 'string' && /^[A-Za-z0-9._-]{1,64}$/.test(f.name)
      && (f.cache === undefined || (Number.isInteger(f.cache) && f.cache >= 16 && f.cache <= 4096)) ? f : null;
  }
  return null;
}

/** A value for an error message, short. */
export function showValue(v) {
  if (typeof v === 'function') return 'a function';
  try {
    const s = JSON.stringify(v);
    if (s !== undefined) return s.length > 60 ? `${s.slice(0, 57)}...` : s;
  } catch { /* below */ }
  return String(v);
}

/** [w, h] of a `size`, true for 'fit', or null if it is not one. */
export function sizeValue(s) {
  if (s === 'fit') return true;
  let wh;
  if (typeof s === 'string') wh = /^\d+x\d+$/.test(s) ? s.split('x').map(Number) : null;
  else if (typeof s === 'object') wh = [Number(s.w ?? s.width), Number(s.h ?? s.height)];
  return wh && wh.every((n) => Number.isInteger(n) && n > 0) ? wh : null;
}
