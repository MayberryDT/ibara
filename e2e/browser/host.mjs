// Native messaging host for reader.mjs: relays Chrome's framed stdio (a
// 4-byte little-endian length, then JSON) to the driver's Unix socket as
// one JSON object per line, and back. It stands where `ibara chrome-host`
// and ibarad's bridge stand.
import net from 'node:net';
const socket = net.createConnection(process.argv.at(-1));
let input = Buffer.alloc(0);
process.stdin.on('data', chunk => {
  input = Buffer.concat([input, chunk]);
  while (input.length >= 4) {
    const length = input.readUInt32LE(0);
    if (input.length < 4 + length) break;
    socket.write(input.subarray(4, 4 + length).toString() + '\n');
    input = input.subarray(4 + length);
  }
});
let line = '';
socket.on('data', chunk => {
  line += chunk;
  for (let end; (end = line.indexOf('\n')) >= 0; line = line.slice(end + 1)) {
    const body = Buffer.from(line.slice(0, end));
    const head = Buffer.alloc(4);
    head.writeUInt32LE(body.length);
    process.stdout.write(Buffer.concat([head, body]));
  }
});
socket.on('close', () => process.exit(0));
process.stdin.on('end', () => process.exit(0));
