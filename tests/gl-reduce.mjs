// Shrink a recorded GL command stream to the shortest sequence that still fails.
//
//   node tests/gl-reduce.mjs TRACE --out MIN --oracle 'CMD {}' [--expect REGEX]
//
// `{}` in the oracle command is replaced with the path of a candidate trace.
// A candidate "still reproduces" when the oracle exits non-zero, or -- when
// --expect is given -- when its output matches that regular expression, which
// is the safer signal: a reduced stream can fail for a new reason, and a
// reduction that chases whatever failure it happens to cause is worthless.
//
// The search is ddmin over the flat command list. Commands are removed whole,
// so every candidate is still a well-framed stream; what it is not is a
// *valid* one -- deleting the creation of an object that a survivor uses is
// exactly the kind of minimal reproducer this is looking for.

import { execSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { flatten, parseTrace, rebuild, writeTrace, op } from "./gl-trace.mjs";

const argv = process.argv.slice(2);
const positional = [];
const flags = new Map();
for (let i = 0; i < argv.length; i++) {
  const arg = argv[i];
  if (!arg.startsWith("--")) {
    positional.push(arg);
    continue;
  }
  const [name, inline] = arg.slice(2).split("=", 2);
  const wantsValue = ["out", "oracle", "expect", "keep-ops", "timeout"].includes(name);
  flags.set(name, inline ?? (wantsValue ? argv[++i] : "1"));
}

const [tracePath] = positional;
const outPath = flags.get("out");
const oracle = flags.get("oracle");
if (!tracePath || !outPath || !oracle) {
  console.error("usage: gl-reduce.mjs TRACE --out MIN --oracle 'CMD {}' [--expect REGEX]");
  console.error("                     [--keep-ops 1,2] [--timeout SECONDS] [--quiet]");
  process.exit(2);
}

const quiet = flags.has("quiet");
const expect = flags.has("expect") ? new RegExp(flags.get("expect")) : null;
const timeout = Number(flags.get("timeout") ?? 300) * 1000;
// Context creation and surface size are not part of any interesting sequence:
// without them nothing runs at all, and a reduction that drops them converges
// on "the trace does nothing", which reproduces everything and explains none
// of it.
const keepOps = new Set(
  (flags.get("keep-ops") ?? `${op.CONTEXT_CREATE},${op.SURFACE_RESIZE}`)
    .split(",")
    .filter((value) => value !== "")
    .map(Number),
);

const trace = parseTrace(new Uint8Array(readFileSync(tracePath)));
const all = flatten(trace);
const pinned = all.filter((c) => keepOps.has(c.op));
const candidates = all.filter((c) => !keepOps.has(c.op));

const work = mkdtempSync(join(tmpdir(), "gex-gl-reduce-"));
const candidatePath = join(work, "candidate.gexgltr");
let attempts = 0;

/** Does this subset of the removable commands still reproduce? */
function reproduces(kept) {
  attempts++;
  // Pinned commands keep their original positions relative to the survivors.
  const keptSet = new Set(kept);
  const ordered = all.filter((c) => keepOps.has(c.op) || keptSet.has(c));
  writeFileSync(candidatePath, writeTrace(rebuild(trace, ordered)));

  const cmd = oracle.replaceAll("{}", candidatePath);
  try {
    const output = execSync(cmd, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"], timeout });
    return expect ? expect.test(output) : false;
  } catch (e) {
    if (e.signal === "SIGTERM" && e.killed) return false; // a hang is not this failure
    const output = `${e.stdout ?? ""}${e.stderr ?? ""}`;
    return expect ? expect.test(output) : true;
  }
}

function report(stage, kept) {
  if (quiet) return;
  console.log(
    `gl-reduce ${stage}: ${pinned.length + kept.length}/${all.length} commands, ${attempts} runs`,
  );
}

if (!reproduces(candidates)) {
  console.error("FAIL gl-reduce: the original trace does not reproduce under this oracle");
  rmSync(work, { recursive: true, force: true });
  process.exit(1);
}
report("start", candidates);

// ddmin: try ever finer partitions, and whenever removing one of them keeps the
// failure, restart from the smaller input at a coarser granularity.
let kept = candidates;
let granularity = 2;
while (kept.length >= 2) {
  const size = Math.ceil(kept.length / granularity);
  let reduced = false;
  for (let start = 0; start < kept.length; start += size) {
    const without = kept.slice(0, start).concat(kept.slice(start + size));
    if (without.length === kept.length) continue;
    if (reproduces(without)) {
      kept = without;
      granularity = Math.max(granularity - 1, 2);
      reduced = true;
      report("shrank", kept);
      break;
    }
  }
  if (reduced) continue;
  if (granularity >= kept.length) break;
  granularity = Math.min(kept.length, granularity * 2);
}

const minimal = rebuild(
  trace,
  all.filter((c) => keepOps.has(c.op) || new Set(kept).has(c)),
);
writeFileSync(outPath, writeTrace(minimal));
rmSync(work, { recursive: true, force: true });

const commands = pinned.length + kept.length;
console.log(
  `PASS gl-reduce ${all.length}->${commands} commands, ` +
    `${trace.batches.length}->${minimal.batches.length} batches, ${attempts} oracle runs`,
);
console.log(`gl-reduce wrote ${outPath}`);
