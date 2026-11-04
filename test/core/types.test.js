// The package's declarations (page/runtime/types/*.d.ts) against the modules
// they describe: every value a module exports is declared and every declared
// one exported; every public method and accessor of each exported class is
// declared, and every declared one is there; System's options are the ones
// its constructor takes. Fields set in a constructor (`readonly x: T`) are
// not checked against the module: they exist only on an instance.
import { readFileSync } from 'node:fs';
import { PACKAGE_ENTRIES } from '../lib/package.mjs';

export const meta = { hosts: ['node'], images: [], timeout: 60 };

const ROOT = new URL('../../page/runtime/', import.meta.url);

/** `{ values, classes: { Name: { methods, fields } }, interfaces: { Name: [keys] } }`
 *  from a d.ts: what is declared at its top level, and each class's members. */
function declarations(text) {
  const values = new Set();
  const classes = {};
  const interfaces = {};
  for (const m of text.matchAll(/^export declare (?:const|function|class|abstract class) (\w+)/gm)) values.add(m[1]);
  for (const m of text.matchAll(/^export \{([^}]*)\}/gm)) for (const n of m[1].split(',')) if (n.trim()) values.add(n.trim().split(/\s+as\s+/).pop());
  const body = (start) => {
    let depth = 0;
    for (let i = start; i < text.length; i++) {
      if (text[i] === '{') depth++;
      else if (text[i] === '}' && --depth === 0) return text.slice(start + 1, i);
    }
    return '';
  };
  // A body's own lines, nested braces (an option bag's type) left out.
  const ownLines = (b) => {
    const out = [];
    let depth = 0;
    let line = '';
    for (const ch of b) {
      if (ch === '{') depth++;
      if (depth === 0) line += ch;
      if (ch === '}') depth--;
      if (ch === '\n') { out.push(line); line = ''; }
    }
    out.push(line);
    return out.map((l) => l.replace(/\/\*\*.*?\*\/|\/\/.*$/g, '').trim()).filter((l) => l && !l.startsWith('*') && !l.startsWith('/*'));
  };
  for (const m of text.matchAll(/^export declare (?:abstract )?class (\w+)[^{]*/gm)) {
    const methods = new Set();
    const fields = new Set();
    for (const l of ownLines(body(m.index + m[0].length))) {
      const g = /^(?:static\s+)?(?:get|set)\s+(\w+)\s*\(/.exec(l);
      const f = /^(?:readonly\s+)?(\w+)\??\s*:/.exec(l);
      const fn = /^(\w+)\s*[(<]/.exec(l);
      if (l.startsWith('constructor')) continue;
      if (g) methods.add(g[1]);
      else if (fn) methods.add(fn[1]);
      else if (f) fields.add(f[1]);
    }
    classes[m[1]] = { methods, fields };
  }
  for (const m of text.matchAll(/^export interface (\w+)[^{]*/gm)) {
    interfaces[m[1]] = ownLines(body(m.index + m[0].length)).map((l) => /^(?:readonly\s+)?(\w+)\??\s*[:(]/.exec(l)?.[1]).filter(Boolean);
  }
  return { values, classes, interfaces };
}

/** A class's public methods and accessors: its own and its bases', up to
 *  the platform's (EventTarget, Event) or another exported class, which
 *  its own declaration (`extends`) answers for. */
function members(C, exported) {
  const out = new Set();
  const stop = [EventTarget.prototype, Event.prototype, Object.prototype];
  for (let p = C.prototype; p && !stop.includes(p) && (p === C.prototype || !exported.has(p.constructor)); p = Object.getPrototypeOf(p)) {
    for (const k of Object.getOwnPropertyNames(p)) if (k !== 'constructor') out.add(k);
  }
  for (const k of Object.getOwnPropertyNames(C)) if (!['length', 'name', 'prototype'].includes(k)) out.add(`static ${k}`);
  return out;
}

export default async function (t) {
  const { eq } = t;
  // Every exported class, for where a class's own members stop.
  const classes = new Set();
  for (const src of Object.keys(PACKAGE_ENTRIES)) {
    for (const v of Object.values(await import(new URL(`src/${src}`, ROOT).href))) if (typeof v === 'function') classes.add(v);
  }
  for (const [src, built] of Object.entries(PACKAGE_ENTRIES)) {
    const dts = readFileSync(new URL(`types/${built.replace(/\.mjs$/, '.d.ts')}`, ROOT), 'utf8');
    const d = declarations(dts);
    const mod = await import(new URL(`src/${src}`, ROOT).href);
    const exported = Object.keys(mod).sort();
    eq(`${built}: the values it exports are the ones declared`, exported, [...d.values].sort());
    for (const name of exported) {
      const C = mod[name];
      if (typeof C !== 'function' || !/^class\b/.test(Function.prototype.toString.call(C))) continue;
      const decl = d.classes[name];
      if (!decl) continue;
      const have = [...members(C, classes)].filter((k) => !k.startsWith('static ')).sort();
      eq(`${built} ${name}: its methods and accessors are the declared ones`, have, [...decl.methods].sort());
    }
  }
  // System's options: the constructor's list, and SystemOptions.
  const dts = declarations(readFileSync(new URL('types/index.d.ts', ROOT), 'utf8'));
  const sys = readFileSync(new URL('src/system.js', ROOT), 'utf8');
  const taken = /\[([^\]]*)\]\.includes\(k\)\) throw new TypeError\(`System: no option/.exec(sys);
  const options = taken ? taken[1].split(',').map((s) => s.trim().replace(/'/g, '')).sort() : null;
  eq('SystemOptions is what new System() takes', (dts.interfaces.SystemOptions ?? []).slice().sort(), options);
}
