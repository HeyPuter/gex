# Gates

The standing regression gates. Each of these was living in a session
scratchpad, which is not a place work survives -- a new session gets an empty
one, and the notes in `docs/gl-webgpu-handoff.md` that tell you to run the
probe suite were pointing at a path that would not exist.

The probe *programs* were always tracked, in `tests/integration/programs`; what
was missing was everything that drives them.

- `probe-suite.sh [name]` — build every `gl44-*-probe.c` and run it twice,
  once on llvmpipe and once on gex_wgpu, comparing the SUMMARY lines. With a
  name, just that one. **Four DIFFs are expected**: `group-vote` (2/3 — the
  extension is claimed and conformant, but its invocation group is one
  invocation, so `all_mixed` answers true where llvmpipe's wider ballot
  answers false; the probe exists to say so), `quad` (gex passes 6/6 where
  llvmpipe passes 3/6 — the reference is the one failing), `texture-sampler`
  (sample-counts 1 vs 2) and `stage-image` (deliberately failing; its comment
  says why for each case). A fifth is a regression.

  `polygon-offset` is deliberately *not* among them. Its two clamp cases are
  written out in a comment rather than run: this driver does not claim
  ARB_polygon_offset_clamp and llvmpipe does, so running them would make the
  probe differ on the case count, which is noise. Paste them back to re-check
  whether the platform has learned the clamp.
- `cts-gate.sh` — a few fast CTS families, chosen because they lean on the
  driver's *internal* buffers, which the probes barely touch. A buffer-growth
  regression once lived through the probe suite, SuperTuxKart and Blender for
  two sessions; one run of this would have caught it the day it landed.
- `app-stk.sh <out.png> [settle]` — one short SuperTuxKart race, photographed.
- `app-blender.sh <out.png> [settle]` — Blender's default solid viewport.
  Compare against `GEX_REF=1` for llvmpipe; it has been pixel-identical.
- `app-blender-eevee.sh` — the EEVEE viewport, which is blocked on WebGPU's
  colour-attachment byte budget.
- `app-minecraft.sh <driver> <out.png> [seconds]` — Minecraft 26.2, which
  needs a GL 4.4 context and so is the widest single statement available.
- `app-compare-top.sh <results.tsv> [count] [outdir]` — re-measure the worst
  rows of a `tests/gl-app-compare.sh` run with both extra references. The cheap
  sweep reports one number and one number cannot tell a driver defect from an
  animation; this adds gex-against-itself (which is animation) and softpipe as
  a second reference (which is the two references disagreeing). **rmse means
  nothing unless it is well above both.** Half the deterministic top of that
  corpus has previously turned out to be llvmpipe rather than the driver.
- `app-candidates.sh [outdir]` — the applications that need arguments, a
  compositor or a file, which is why the broad sweep in `tests/gl-app-sweep.sh`
  skips them.

`run-native.sh` and `killmatch.py` are helpers the others use. Read the top of
each script: they carry the reasons for what look like arbitrary choices.
