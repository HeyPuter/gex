// The checks app.js's `start()` makes of its options before it fetches
// anything: an unknown name (a typo would otherwise be ignored) or a value
// of the wrong kind is an error that names it. No DOM, so Node can test it
// (web/test/check-start-options.mjs).

/** Every option `start` reads (app.js lists what each is). */
export const OPTIONS = [
  'container', 'canvas', 'size', 'args', 'env', 'cwd', 'exe', 'uid', 'mtimes', 'persist', 'wisp',
  'pageHosts', 'strace', 'trapstack', 'debug', 'compositor', 'gl', 'translation', 'hostx', 'mem64',
  'glLazy', 'baseUrl', 'launch', 'build', 'onLog', 'onExit', 'onProgress', 'ready',
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
  if (given('hostx') && ![true, false, 'rootful'].includes(opts.hostx)) bad('hostx', "true, false or 'rootful'");
  if (given('persist') && opts.persist !== false && !(typeof opts.persist === 'object' && typeof opts.persist.dir === 'string')) bad('persist', 'false, or {dir, db}');
  for (const k of ['onLog', 'onExit', 'onProgress']) if (given(k) && typeof opts[k] !== 'function') bad(k, 'a function');
  for (const k of ['debug', 'pageHosts']) if (given(k) && typeof opts[k] !== 'object') bad(k, 'an object, {name: value}');
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
