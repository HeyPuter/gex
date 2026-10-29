// X11 for every windows part: display :0 held by the runtime's Worker until
// the first X client connects, then the runtime's X server (web/src/hostx.js,
// through runProgram's `startX`) started on those sockets, where the client
// waits.

/** Arm it for a system's internals `i`, once the system runs. */
export function xOnDemand(i) {
  i.afterBoot(async (sys) => {
    if (!sys.startX) return;
    const ids = await i.io().call('holdX');
    i.on('x-wanted', () => {
      sys.startX(ids).catch((e) => i.say(`gex: the X server would not start: ${e?.message ?? e}\n`));
    });
  });
}
