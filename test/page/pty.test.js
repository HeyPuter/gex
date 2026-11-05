// Pseudoterminals (fixtures/native/pty/openpty.c): a terminal emulator's pty
// end to end, a child on the slave as its controlling terminal, the line
// discipline's echo and ONLCR, the hangup; natively and in the page, each
// matching the host.
import { hostMatch } from './hostmatch.mjs';

export const meta = { hosts: ['node'], timeout: 600 };

export default (t) => hostMatch(t, 'pty/openpty.c');
