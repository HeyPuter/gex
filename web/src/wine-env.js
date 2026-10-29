// Wine's environment as a page starts it, past what the build and the caller
// gave.

/** A prefix this visit does not keep (no persisted directory, `keep`'s
 *  `dir`, holds its registry files): `WINEGEX_NO_REGSAVE=1` into `env`, so
 *  the wineserver saves no registry every 30 s. Each save held the server,
 *  and every request waiting on it, for tens of ms (an audio underrun) for
 *  files nothing reads again. A kept prefix is saved as before: those saves
 *  are what the persisted directory's snapshots take. Nothing without a
 *  WINEPREFIX, or when the caller set the knob. */
export function noRegSave(env, keep) {
  const prefix = env.find((e) => e.startsWith('WINEPREFIX='))?.slice(11).replace(/\/+$/, '');
  if (!prefix || env.some((e) => e.startsWith('WINEGEX_NO_REGSAVE='))) return env;
  const dir = keep?.dir?.replace(/\/+$/, '');
  const kept = dir !== undefined && (dir === '' || prefix === dir || prefix.startsWith(`${dir}/`));
  if (!kept) env.push('WINEGEX_NO_REGSAVE=1');
  return env;
}
