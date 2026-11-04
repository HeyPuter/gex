// WebAudio: a program's /dev/dsp played by the page, at the player's pace.
import { System } from '../../page/runtime/src/index.js';
import { MemoryFilesystem } from '../../page/runtime/src/fs.js';
import { WebAudio } from '../../page/runtime/src/audio.js';

export const meta = { hosts: ['browser'], images: ['busybox'], timeout: 180 };

export default async function (t) {
  const { say, check, eq, timeout, until } = t;
  const sys = new System({ baseUrl: t.baseUrl, debug: t.debug() });
  const fs = new MemoryFilesystem(sys);
  fs.addTar(t.work('bb.tar.zst'));
  await timeout(sys.boot(), 60000, 'boot');
  const env = { PATH: '/bin:/usr/bin', HOME: '/root' };

  // No player yet: the program still finishes.
  const before = await timeout(sys.run(['rt-probe', 'tone', '440', '1500'], { env }), 30000, 'tone');
  eq('with no player, /dev/dsp takes it', before.code, 0);
  say(`  1500 ms written in ${before.stdout.trim()} ms with no player (the wall clock)`);
  check('and nothing plays it', globalThis.__gexAudio == null);

  const audio = new WebAudio(sys);
  await timeout(audio.ready, 10000, 'player');
  check('a context', audio.context instanceof AudioContext, String(audio.context));
  await until(() => audio.context.state === 'running', 'the context runs', 5000);
  check('a second throws', (() => { try { new WebAudio(sys); return false; } catch { return true; } })());
  const tone = await timeout(sys.run(['rt-probe', 'tone', '440', '1500'], { env }), 30000, 'tone');
  eq('a tone', tone.code, 0);
  const ms = Number(tone.stdout);
  // Paced, less what the device queues ahead.
  // (No upper bound for a loaded machine's sake beyond "not stuck".)
  check(`1500 ms written in ${ms} ms with the player`, ms > 300 && ms < 15000, tone.stdout);
  // The program is done once the device has taken the tone; the player is
  // still playing what it queued, and says what it played about twice a
  // second, later on a busy machine: wait for its word.
  const loudNow = () => ((audio.context && globalThis.__gexAudio?.history) ?? []).some((r) => r.rms > 0.05);
  await until(loudNow, 'the player\'s report', 10000).catch(() => {});
  const heard = (audio.context && globalThis.__gexAudio?.history) ?? [];
  const loud = heard.filter((r) => r.rms > 0.05);
  check('it played', loud.length > 0, JSON.stringify(heard.slice(-3)));
  const hz = loud.map((r) => r.hz).sort((a, b) => a - b)[loud.length >> 1];
  check(`at 440 Hz (${hz?.toFixed(0)})`, Math.abs(hz - 440) < 30);
  say(`  backlog ${JSON.stringify(audio.stats?.backlog)}, base ${audio.stats?.base} ms, output ${audio.stats?.output} ms`);
  await audio.close();
  eq('closed', audio.context, null);

  await sys.shutdown();
}
