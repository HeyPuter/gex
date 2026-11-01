import type { Duplex, Network } from './net';

/** A network over a Wisp proxy (MercuryWorkshop's wisp-js client): every
 *  connection on one WebSocket, dialled again when it drops. Events: `open`
 *  and `closed` as the WebSocket comes and goes. */
export declare class Wisp extends EventTarget implements Network {
  /** The proxy at `url` (`ws:` or `wss:`), dialled at the first connection. */
  constructor(url: string | URL);
  get url(): string;
  connect(host: string, port: number): Promise<Duplex>;
  /** A Wisp UDP stream (refused by a proxy without Wisp's UDP extension). */
  datagram(host: string, port: number): Promise<Duplex>;
  /** Closes the WebSocket; the next connection dials again. */
  close(): void;
}
