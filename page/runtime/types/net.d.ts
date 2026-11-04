/** One connection: the far end's bytes and the program's. The page ending
 *  `readable` is the far end closing; the program closing its socket closes
 *  `writable` and cancels `readable`. */
export interface Duplex {
  readable: ReadableStream<Uint8Array>;
  writable: WritableStream<Uint8Array>;
}

/** Where a system's programs' connections out go (`System.network`): the
 *  page's to implement, over whatever transport it has. */
export interface Network {
  /** A program's connection to `host` (an address, or a name the program
   *  did not look up first) at `port`. A throw or a rejection refuses it. */
  connect(host: string, port: number): Duplex | Promise<Duplex>;
  /** A program's UDP to `host` at `port`: a flow whose chunks are
   *  datagrams, each written one the program sent and each read one back
   *  from there. Without it, UDP out is unreachable. A throw or a rejection
   *  drops what was sent. */
  datagram?(host: string, port: number): Duplex | Promise<Duplex>;
}
