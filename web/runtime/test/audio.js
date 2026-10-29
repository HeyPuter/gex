// WebAudio: a program's /dev/dsp played by the page, at the player's pace.
import { System } from '../src/index.js';
import { Filesystem } from '../src/fs.js';
import { WebAudio } from '../src/audio.js';
import { main, say, check, eq, timeout, testDebug, until } from './lib.js';

main(async () => {
  const sys = new System({ baseUrl: '/', debug: testDebug() });
  const fs = new Filesystem(sys);
  fs.addTar('/__rt/bb.tar.zst');
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
  const t = await timeout(sys.run(['rt-probe', 'tone', '440', '1500'], { env }), 30000, 'tone');
  eq('a tone', t.code, 0);
  const ms = Number(t.stdout);
  // Paced, less what the device queues ahead.
  check(`1500 ms written in ${ms} ms with the player`, ms > 300 && ms < 4000, t.stdout);
  const heard = (audio.context && globalThis.__gexAudio?.history) ?? [];
  const loud = heard.filter((r) => r.rms > 0.05);
  check('it played', loud.length > 0, JSON.stringify(heard.slice(-3)));
  const hz = loud.map((r) => r.hz).sort((a, b) => a - b)[loud.length >> 1];
  check(`at 440 Hz (${hz?.toFixed(0)})`, Math.abs(hz - 440) < 30);
  say(`  backlog ${JSON.stringify(audio.stats?.backlog)}, base ${audio.stats?.base} ms, output ${audio.stats?.output} ms`);
  await audio.close();
  eq('closed', audio.context, null);

  await sys.shutdown();
});
