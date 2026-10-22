// How much of a batch's reply goes back into the guest's ring.
//
// The ring's reply area has a fixed capacity (guest/gl/gexgl.c: 32 MiB).
// When the replies of one batch add up to more, the native host
// (shims/native/src/gl.rs) copies what fits and reports success, and so
// does the page: BAD_ARGUMENT would make the guest drop the whole queued
// batch and every one after it, and a game that runs natively would show a
// black window in the page. The reply is cut to the capacity, and the cut is counted
// so the page's GL summary names it.

/** The reply length to copy, and whether it was cut. */
export function fitReply(replyLen, replyCap) {
  return replyLen > replyCap ? { len: replyCap, cut: replyLen - replyCap } : { len: replyLen, cut: 0 };
}
