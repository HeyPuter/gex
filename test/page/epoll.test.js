// An epoll descriptor in a poll set (fixtures/native/epoll/pollep.c), as
// GLib's main loop polls a library's epoll: with nothing ready the poll waits
// out its timeout (it once came back at once, and kded6 and plasmashell spun a
// core each), written it wakes; natively and in the page, each matching the
// host.
import { hostMatch } from './hostmatch.mjs';

export const meta = { hosts: ['node'], timeout: 600 };

export default (t) => hostMatch(t, 'epoll/pollep.c');
