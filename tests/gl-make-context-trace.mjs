// Write a one-batch .gexgltr that asks for a particular GL context and does
// nothing else.
//
//   node tests/gl-make-context-trace.mjs OUT [es|core] [major] [minor]
//
// The backend creates its context from the stream's own CONTEXT_CREATE -- only
// the guest knows which API it bound -- so measuring what a *desktop* context
// reaches needs a stream that asks for one. Replaying this prints the driver's
// version ladder without needing a guest program that speaks desktop GL.
import { writeFileSync } from 'node:fs';

const [out, api = 'es', major = '1', minor = '0'] = process.argv.slice(2);
if (!out) {
  console.error('usage: gl-make-context-trace.mjs OUT [es|core] [major] [minor]');
  process.exit(2);
}

const PROTOCOL_VERSION = 6;   // crates/gex-gl/src/proto.rs
const CONTEXT_CREATE = 1;
const CLIENT_API = { es: 0, core: 1 };
if (!(api in CLIENT_API)) {
  console.error(`unknown client API "${api}"; use es or core`);
  process.exit(2);
}

// { client_api, major, minor }, in the command framing the decoder expects:
// opcode, total byte length, then the payload words.
const command = Buffer.alloc(8 + 3 * 4);
command.writeUInt32LE(CONTEXT_CREATE, 0);
command.writeUInt32LE(command.length, 4);
command.writeUInt32LE(CLIENT_API[api], 8);
command.writeUInt32LE(Number(major), 12);
command.writeUInt32LE(Number(minor), 16);

const header = Buffer.alloc(24);
header.write('GEXGLTR\0', 0, 'latin1');
header.writeUInt32LE(1, 8);                  // format
header.writeUInt32LE(PROTOCOL_VERSION, 12);
header.writeUInt32LE(64, 16);                // width
header.writeUInt32LE(64, 20);                // height

const record = Buffer.alloc(8);
record.writeUInt32LE(command.length, 0);
record.writeUInt32LE(4096, 4);               // reply capacity

writeFileSync(out, Buffer.concat([header, record, command]));
console.log(`wrote ${out}: ${api} ${major}.${minor}`);
