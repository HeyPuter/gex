// PowerPC (ppc32) tier-1 against emcc, next to the AArch64 guest, on pewasm's
// Tier-0 corpus (docs/ppc.md §2.3, §13). S1's rules: every config in a fresh
// process, rounds in rotating order, the headline the median of per-round
// ratios against emcc, the noise the median absolute deviation of those
// ratios, and every run's checksum must equal emcc's. A null config (emcc
// against itself) says how much a ratio can move by noise alone.
//
//   node tests/ppc/bench.mjs --gp32 DIR [--gp32<x> DIR]... --ga64 DIR
//        --shim-ppc WASM --shim-a64 WASM [--reps 5] [--skip a,b] [programs...]
//
// DIR holds <name> (the ELF) and <name>.wasm (`gexc build --exhaustive
// --no-shared-memory`); every --gp32<x> is another ppc32 build (with
// GEX_BSWAP_BALLAST=1, say) timed in the same rounds. Emcc's builds come from
// pewasm's bench/out/b1 (PEWASM, default ~/src/noita-wasm/pewasm).
import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const PEWASM = process.env.PEWASM ?? join(process.env.HOME, 'src/noita-wasm/pewasm');
const { scaleOf, allPrograms, median } = await import(join(PEWASM, 'bench/lib.mjs'));
const GEX = fileURLToPath(new URL('../..', import.meta.url));
const RUN = join(PEWASM, 'bench/gex-run.mjs');

const argv = process.argv.slice(2);
const opt = { reps: 5, programs: [] };
for (let i = 0; i < argv.length; i++) {
    const k = argv[i];
    if (k.startsWith('--')) opt[k.slice(2)] = k === '--reps' ? Number(argv[++i]) : argv[++i];
    else opt.programs.push(k);
}
const programs = opt.programs.length ? opt.programs : allPrograms('0');

// Every --gp32* DIR is a ppc32 build (run with --shim-ppc), --ga64 DIR the
// AArch64 one (--shim-a64); `null` is emcc against itself.
const configs = { null: (n, s) => [process.execPath, [join(PEWASM, 'bench/out/b1', `${n}.js`), String(s)]] };
for (const [k, dir] of Object.entries(opt)) {
    const shim = k.startsWith('gp32') ? opt['shim-ppc'] : k === 'ga64' ? opt['shim-a64'] : null;
    if (!shim) continue;
    configs[k] = (n, s) => [process.execPath, [RUN, GEX, shim, join(dir, `${n}.wasm`), join(dir, n), String(s)]];
}
const skip = new Set((opt.skip ?? '').split(',').filter(Boolean));
const b1 = (n, s) => [process.execPath, [join(PEWASM, 'bench/out/b1', `${n}.js`), String(s)]];

function once([cmd, args]) {
    const r = spawnSync(cmd, args, { encoding: 'utf8', maxBuffer: 1 << 26, timeout: 900_000 });
    const out = r.stdout + r.stderr;
    const us = out.match(/kernel-us (\d+)/), result = out.match(/result (\d+)/);
    if (!us || !result) throw new Error(out.slice(-600));
    return { us: Number(us[1]), result: result[1] };
}

const names = Object.keys(configs);
const geo = Object.fromEntries(names.map(c => [c, []]));
console.log(`program    ${names.map(c => `${c}/emcc`.padStart(18)).join('')}`);
for (const name of programs) {
    if (skip.has(name)) continue;
    const scale = scaleOf(name);
    if (!existsSync(join(PEWASM, 'bench/out/b1', `${name}.js`))) continue;
    const ratios = Object.fromEntries(names.map(c => [c, []]));
    let bad = null;
    try {
        for (let i = 0; i < opt.reps; i++) {
            // Rotate which config runs first, so drift does not favour one.
            const order = ['b1', ...names];
            const rot = order.slice(i % order.length).concat(order.slice(0, i % order.length));
            const round = {};
            for (const c of rot) round[c] = once(c === 'b1' ? b1(name, scale) : configs[c](name, scale));
            for (const c of names) {
                if (round[c].result !== round.b1.result) bad = `${c}: ${round[c].result} != ${round.b1.result}`;
                ratios[c].push(round[c].us / round.b1.us);
            }
        }
    } catch (e) {
        console.log(`${name.padEnd(10)} ERROR ${e.message.split('\n').slice(-3).join(' | ')}`);
        continue;
    }
    const cells = names.map(c => {
        const m = median(ratios[c]), mad = median(ratios[c].map(r => Math.abs(r - m)));
        geo[c].push(m);
        return `${m.toFixed(3)} ±${(100 * mad / m).toFixed(1)}%`.padStart(18);
    });
    console.log(`${name.padEnd(10)} ${cells.join('')}${bad ? `  MISMATCH ${bad}` : ''}`);
}
const g = v => Math.exp(v.reduce((s, x) => s + Math.log(x), 0) / v.length);
console.log(`geomean    ${names.map(c => g(geo[c]).toFixed(3).padStart(18)).join('')}  (${geo[names[0]].length} programs)`);
