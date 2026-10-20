// The far end for mem64-net.c: a TCP server on 127.0.0.1 that answers every
// byte it receives XOR 0x5a, for any number of connections, until killed.
// Prints its port on the first line.
import net from 'node:net';
const server = net.createServer((c) => {
  c.on('data', (d) => {
    for (let i = 0; i < d.length; i++) d[i] ^= 0x5a;
    c.write(d);
  });
  c.on('error', () => {});
});
server.listen(0, '127.0.0.1', () => console.log(server.address().port));
