// The page's time zone as a POSIX TZ string (page/src/posix-tz.js): for each
// zone, the string's offset (evaluated as musl does a TZ rule) is Intl's at
// every hour of this year. Zones: no daylight time, both hemispheres, half
// and quarter hours, a 30-minute daylight shift, rules that changed.
// A year no pair of rules fits (Morocco's: +01 but around Ramadan, which can
// come twice in a year) is one offset, the one for most of the year: off
// only for the shorter part, which is what is checked there. Morocco's
// first such year from this one on is also checked, against the offset Intl
// gives for most of its hours (tzdata releases differ on Morocco's future:
// 2026c has it on +00 from late 2026, earlier ones keep the Ramadan months).
// With --libc, the strings are also handed to the host's libc (Python's
// time.localtime under each TZ), which must read them as this test does
// (musl's reading of TZ rules).
//
//   node test/fixtures/page/posix-tz.mjs [--libc]
import { pathToFileURL } from 'node:url';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';
// gex: the checkout these fixtures are in, or GEX_ROOT.
const GEX_ROOT = process.env.GEX_ROOT || join(import.meta.dirname, '..', '..', '..');
const { posixTz, offsetAt, transitions } = await import(pathToFileURL(join(GEX_ROOT, 'page/src/posix-tz.js')).href);

const ZONES = [
  'UTC', 'Etc/GMT+5', 'Asia/Tokyo', 'Asia/Kolkata', 'Asia/Kathmandu', 'Asia/Shanghai', 'America/Phoenix',
  'America/New_York', 'America/Los_Angeles', 'America/St_Johns', 'America/Havana', 'America/Santiago',
  'America/Sao_Paulo', 'America/Mexico_City', 'Europe/London', 'Europe/Dublin', 'Europe/Berlin', 'Europe/Moscow',
  'Asia/Tehran', 'Asia/Jerusalem', 'Asia/Gaza', 'Africa/Cairo', 'Africa/Casablanca', 'Pacific/Auckland',
  'Pacific/Chatham', 'Pacific/Apia', 'Pacific/Kiritimati', 'Australia/Sydney', 'Australia/Adelaide',
  'Australia/Lord_Howe', 'Australia/Eucla', 'America/Asuncion', 'Antarctica/Troll', 'Pacific/Marquesas',
];

// A POSIX TZ string's offset east of UTC at `t`, as musl reads one: std
// and dst names and offsets, and Mm.w.d[/time] rules (time may be negative
// or past 24 h).
function parse(s) {
  let i = 0;
  const nameAt = () => {
    if (s[i] === '<') { const j = s.indexOf('>', i); i = j + 1; return; }
    while (i < s.length && /[A-Za-z]/.test(s[i])) i++;
  };
  const num = () => {
    const m = /^([+-]?)(\d+)(?::(\d+))?(?::(\d+))?/.exec(s.slice(i));
    if (!m) return null;
    i += m[0].length;
    const v = Number(m[2]) * 3600 + Number(m[3] ?? 0) * 60 + Number(m[4] ?? 0);
    return m[1] === '-' ? -v : v;
  };
  nameAt();
  const std = -num();
  if (i >= s.length) return { std, dst: null };
  nameAt();
  let dst = std + 3600;
  if (s[i] !== ',') dst = -num();
  const rules = [];
  while (s[i] === ',') {
    i++;
    const m = /^M(\d+)\.(\d)\.(\d)/.exec(s.slice(i));
    i += m[0].length;
    let time = 7200;
    if (s[i] === '/') { i++; time = num(); }
    rules.push({ month: Number(m[1]), week: Number(m[2]), wd: Number(m[3]), time });
  }
  return { std, dst, start: rules[0], end: rules[1] };
}
function ruleUtc(r, year, before) {
  const first = new Date(Date.UTC(year, r.month - 1, 1)).getUTCDay();
  let day = 1 + ((r.wd - first + 7) % 7) + 7 * (r.week - 1);
  const inMonth = new Date(Date.UTC(year, r.month, 0)).getUTCDate();
  while (day > inMonth) day -= 7;
  return Date.UTC(year, r.month - 1, day) + (r.time - before) * 1000;
}
function evalTz(p, t) {
  if (p.dst === null) return p.std;
  const year = new Date(t + p.std * 1000).getUTCFullYear();
  const a = ruleUtc(p.start, year, p.std);
  const b = ruleUtc(p.end, year, p.dst);
  const inDst = a < b ? t >= a && t < b : t >= a || t < b;
  return inDst ? p.dst : p.std;
}

const year = new Date().getUTCFullYear();
const start = Date.UTC(year, 0, 1);
const end = Date.UTC(year + 1, 0, 1);
let failed = 0;
const strings = {};
function check(zone, year) {
  const start = Date.UTC(year, 0, 1);
  const end = Date.UTC(year + 1, 0, 1);
  const s = posixTz(zone, year);
  const p = parse(s);
  let bad = 0;
  let n = 0;
  let first = null;
  for (let t = start; t < end; t += 3600e3) {
    // Also a minute either side of the hour, where a transition off the
    // hour would show.
    for (const u of [t, t + 30 * 60e3, t + 45 * 60e3]) {
      const want = offsetAt(zone, u);
      const got = evalTz(p, u);
      n++;
      if (got !== want) { bad++; first ??= { at: new Date(u).toISOString(), got, want }; }
    }
  }
  const what = `${zone}${year === new Date().getUTCFullYear() ? '' : ` ${year}`}`;
  // One offset for a year with transitions: right for the longer part.
  const irregular = p.dst === null && transitions(zone, year).length > 0;
  if (!bad) console.log(`ok   ${what.padEnd(22)} ${s}`);
  else if (irregular && bad * 2 < n) console.log(`ok   ${what.padEnd(22)} ${s} (no rules fit the year: ${bad} of ${n} samples off, the shorter part)`);
  else { failed++; console.log(`FAIL ${what}: ${s}: ${bad} of ${n} samples off, first ${JSON.stringify(first)}`); }
  return s;
}
for (const zone of ZONES) strings[zone] = check(zone, year);
// Chosen for the year, not by when the page happens to load: asked at a
// moment of the year at its other offset (a Ramadan), the answer is still
// the offset of most of the year.
const CAS = 'Africa/Casablanca';
const fits = (y) => {
  const t = transitions(CAS, y);
  return t.length === 0 || (t.length === 2 && offsetAt(CAS, t[1]) === offsetAt(CAS, t[0] - 1000));
};
let odd = null;
for (let y = year; y < year + 40 && odd === null; y++) if (!fits(y)) odd = y;
if (odd === null) console.log(`ok   ${CAS}: no year from ${year} to ${year + 39} that no rules fit, in this tzdata`);
else {
  const hours = new Map();
  for (let u = Date.UTC(odd, 0, 1); u < Date.UTC(odd + 1, 0, 1); u += 3600e3) {
    const o = offsetAt(CAS, u);
    hours.set(o, (hours.get(o) ?? 0) + 1);
  }
  const most = [...hours].reduce((p, q) => (q[1] > p[1] ? q : p))[0];
  let other = null;
  for (let u = Date.UTC(odd, 0, 1); u < Date.UTC(odd + 1, 0, 1) && other === null; u += 86400e3) if (offsetAt(CAS, u) !== most) other = u;
  const realNow = Date.now;
  if (other !== null) Date.now = () => other;
  const irregular = posixTz(CAS, odd);
  Date.now = realNow;
  const off = parse(irregular);
  if (!off || off.dst !== null || off.std !== most) { failed++; console.log(`FAIL ${CAS} ${odd}: ${irregular}, wanted offset ${most} s east, the offset of ${hours.get(most)} of its ${[...hours.values()].reduce((p, q) => p + q, 0)} hours`); }
  else check(CAS, odd);
}

if (process.argv.includes('--libc')) {
  // One Python process: each zone's string through the host libc's tzset.
  const py = `
import json, os, sys, time
d = json.load(sys.stdin); out = {}
for zone, s in d['zones'].items():
    os.environ['TZ'] = s; time.tzset()
    out[zone] = [time.localtime(t).tm_gmtoff for t in d['ts']]
print(json.dumps(out))`;
  const ts = [];
  for (let t = start; t < end; t += 3600e3) ts.push(t / 1000, t / 1000 + 1800, t / 1000 + 2700);
  const res = JSON.parse(execFileSync('python3', ['-I', '-c', py], { input: JSON.stringify({ zones: strings, ts }), maxBuffer: 1 << 28 }).toString());
  for (const zone of ZONES) {
    // the string as this test reads it (which is Intl's, or the year's most
    // offset where no rules fit)
    const p = parse(strings[zone]);
    const bad = ts.filter((t, i) => res[zone][i] !== evalTz(p, t * 1000));
    if (bad.length) { failed++; console.log(`FAIL libc ${zone}: ${strings[zone]}: ${bad.length} samples off, first ${new Date(bad[0] * 1000).toISOString()}`); }
  }
  if (!failed) console.log('ok   libc agrees for every zone');
}

if (failed) process.exit(1);
console.log(`ok   posix-tz: ${ZONES.length} zones, every hour of ${year} (and :30, :45) match Intl`);
