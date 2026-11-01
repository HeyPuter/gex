/** One connection: the far end's bytes and the program's. The page ending
 *  `readable` is the far end closing; the program closing its socket closes
 *  `writable` and cancels `readable`. */
export interface Duplex {
  readable: ReadableStream<Uint8Array>;
  writable: WritableStream<Uint8Array>;
}

/** Where a system's programs' connections out go (`System.network`): the
 *  page's to implement, over whatever transport it has. TCP only. */
export interface Network {
  /** A program's connection to `host` (an address, or a name the program
   *  did not look up first) at `port`. A throw or a rejection refuses it. */
  connect(host: string, port: number): Duplex | Promise<Duplex>;
}
