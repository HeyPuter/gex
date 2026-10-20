// Which programs get the TSO flavour (web/src/runtime.js detectTso): none by default,
// with tso=arm only on an arm64 host, with tso=1 / tso=0 always / never. The host is faked
// through navigator.userAgentData; switches arrive as GEX_* under Node.
//   node tests/browser-tso-select.mjs
const runtime = new URL("../web/src/runtime.js", import.meta.url).href;
const cases = [];
const set = (arch) => Object.defineProperty(globalThis, 'navigator', { configurable: true, value: arch === undefined ? {} : { userAgentData: { getHighEntropyValues: async () => ({ architecture: arch }) } } });
const { detectTso } = await import(runtime);
for (const [sw, arch, want] of [[undefined, 'arm', false], [undefined, 'x86', false], ['arm', 'arm', true], ['arm', 'x86', false], ['arm', undefined, false], ['1', 'x86', true], ['0', 'arm', false]]) {
  if (sw === undefined) delete process.env.GEX_TSO; else process.env.GEX_TSO = sw;
  set(arch);
  const got = await detectTso();
  cases.push(`${got === want ? 'ok  ' : 'FAIL'} tso=${sw ?? '(none)'} host=${arch ?? '(unknown)'}: ${got}`);
}
console.log(cases.join('\n'));
process.exit(cases.some((c) => c.startsWith('FAIL')) ? 1 : 0);
