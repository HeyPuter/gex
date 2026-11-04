// The browser's time zone as a POSIX TZ string ("EST5EDT"-style, with this
// year's rules), for a guest libc that has no zoneinfo files: musl reads the
// rules from the string itself, and Wine takes its clock (and the Windows
// zone it matches) from the libc's localtime.
//
// The rules are this year's transitions, as Intl gives them: none is a zone
// on one offset; two (into the zone's daylight time and out) are a rule of
// the form Mm.w.d/time, the week 5 ("last") when the day is in the month's
// last seven. A year no such pair fits (a rule changed, or Morocco's: +01
// but for a month around Ramadan, which moves with the moon and can come
// twice in a year) is the offset the zone keeps for most of the year,
// without daylight time: an hour off for the shorter part of it, never the
// longer.

const DAY = 86400e3;
const formats = new Map();

/** The zone's offset east of UTC at `t` (Unix ms), in seconds. */
export function offsetAt(zone, t) {
  let f = formats.get(zone);
  if (!f) {
    f = new Intl.DateTimeFormat('en-US', {
      timeZone: zone, hourCycle: 'h23', year: 'numeric', month: 'numeric', day: 'numeric',
      hour: 'numeric', minute: 'numeric', second: 'numeric', era: 'short',
    });
    formats.set(zone, f);
  }
  const s = Math.floor(t / 1000) * 1000;
  const p = {};
  for (const { type, value } of f.formatToParts(new Date(s))) p[type] = value;
  const year = p.era === 'BC' || p.era === 'B' ? 1 - Number(p.year) : Number(p.year);
  const d = new Date(0);
  d.setUTCFullYear(year, Number(p.month) - 1, Number(p.day));
  d.setUTCHours(Number(p.hour) % 24, Number(p.minute), Number(p.second), 0);
  return Math.round((d.getTime() - s) / 1000);
}

/** The instants (Unix ms, to the second) in UTC year `year` where the
 *  zone's offset changes. */
export function transitions(zone, year) {
  const out = [];
  const end = Date.UTC(year + 1, 0, 1);
  let a = Date.UTC(year, 0, 1);
  let oa = offsetAt(zone, a);
  while (a < end) {
    const b = Math.min(a + DAY, end);
    const ob = offsetAt(zone, b);
    if (ob !== oa) {
      // The first second at the new offset.
      let lo = a;
      let hi = b;
      while (hi - lo > 1000) {
        const mid = lo + Math.floor((hi - lo) / 2000) * 1000;
        if (offsetAt(zone, mid) === oa) lo = mid;
        else hi = mid;
      }
      out.push(hi);
    }
    a = b;
    oa = ob;
  }
  return out;
}

/** `[-]h[:mm[:ss]]` of a count of seconds. */
function hms(secs) {
  const sign = secs < 0 ? '-' : '';
  let v = Math.abs(secs);
  const h = Math.floor(v / 3600);
  v -= h * 3600;
  const m = Math.floor(v / 60);
  const s = v - m * 60;
  return `${sign}${h}${m || s ? `:${String(m).padStart(2, '0')}` : ''}${s ? `:${String(s).padStart(2, '0')}` : ''}`;
}

/** A name for an offset east of UTC: `<+0530>`, as tzdata's numeric ones. */
function name(east) {
  const v = Math.abs(east);
  const h = String(Math.floor(v / 3600)).padStart(2, '0');
  const m = Math.floor((v % 3600) / 60);
  const s = v % 60;
  return `<${east < 0 ? '-' : '+'}${h}${m || s ? String(m).padStart(2, '0') : ''}${s ? String(s).padStart(2, '0') : ''}>`;
}

/** A transition at `t` from offset `before` as a POSIX rule: the local date
 *  and time it happens at, on the clock before it. */
function rule(t, before) {
  const local = new Date(t + before * 1000);
  const month = local.getUTCMonth();
  const day = local.getUTCDate();
  const inMonth = new Date(Date.UTC(local.getUTCFullYear(), month + 1, 0)).getUTCDate();
  const week = day + 7 > inMonth ? 5 : Math.ceil(day / 7);
  const secs = Math.round((local.getTime() - Date.UTC(local.getUTCFullYear(), month, day)) / 1000);
  return `M${month + 1}.${week}.${local.getUTCDay()}${secs === 7200 ? '' : `/${hms(secs)}`}`;
}

/** The offset zone `zone` is at for the most time in year `year`, whose
 *  transitions are `t`. */
function mostOfYear(zone, year, t) {
  const edges = [Date.UTC(year, 0, 1), ...t, Date.UTC(year + 1, 0, 1)];
  const time = new Map();
  for (let i = 0; i + 1 < edges.length; i++) {
    const o = offsetAt(zone, edges[i]);
    time.set(o, (time.get(o) ?? 0) + edges[i + 1] - edges[i]);
  }
  return [...time].reduce((a, b) => (b[1] > a[1] ? b : a))[0];
}

/** Zone `zone` (an IANA name) as a POSIX TZ string for year `year` (this
 *  one's by default), or null when Intl does not know it. */
export function posixTz(zone, year = new Date().getUTCFullYear()) {
  let t;
  try {
    t = transitions(zone, year);
  } catch {
    return null;
  }
  const off = (east) => hms(-east);
  if (t.length === 2) {
    const o0 = offsetAt(zone, t[0] - 1000);
    const o1 = offsetAt(zone, t[0]);
    if (offsetAt(zone, t[1]) === o0 && o1 !== o0) {
      const std = Math.min(o0, o1);
      const dst = Math.max(o0, o1);
      // Into daylight time, then out (in either order through the year).
      const [into, out] = o1 > o0 ? [t[0], t[1]] : [t[1], t[0]];
      return `${name(std)}${off(std)}${name(dst)}${off(dst)},${rule(into, std)},${rule(out, dst)}`;
    }
  }
  const most = mostOfYear(zone, year, t);
  return `${name(most)}${off(most)}`;
}
