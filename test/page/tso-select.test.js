// Which programs get the TSO flavour (web/src/runtime.js detectTso): none by
// default, with tso=arm only on an arm64 host, with tso=1 / tso=0 always /
// never. The host is faked through navigator.userAgentData; switches arrive
// as GEX_* under Node.
import { detectTso } from '../../web/src/runtime.js';

export const meta = { hosts: ['node'], arches: ['a64'], timeout: 60 };

const CASES = [[undefined, 'arm', false], [undefined, 'x86', false], ['arm', 'arm', true], ['arm', 'x86', false], ['arm', undefined, false], ['1', 'x86', true], ['0', 'arm', false]];

export default async function (t) {
  for (const [sw, arch, want] of CASES) {
    if (sw === undefined) delete process.env.GEX_TSO;
    else process.env.GEX_TSO = sw;
    Object.defineProperty(globalThis, 'navigator', {
      configurable: true,
      value: arch === undefined ? {} : { userAgentData: { getHighEntropyValues: async () => ({ architecture: arch }) } },
    });
    t.eq(`tso=${sw ?? '(none)'} host=${arch ?? '(unknown)'}`, await detectTso(), want);
  }
}
