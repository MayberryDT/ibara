// Development-only generator for canonical_vectors.json (Node is never used at run time).
// `canonicalize` is copied verbatim from controller/src/journal.ts:118-128 and the
// fingerprint from journal.ts:192-194. Regenerate with:
//   node src/store/testdata/gen_canonical_vectors.mjs > src/store/testdata/canonical_vectors.json
import { createHmac } from 'node:crypto';

function canonicalize(value) {
  if (value === null || typeof value === 'number' || typeof value === 'boolean' || typeof value === 'string') {
    return JSON.stringify(value);
  }
  if (Array.isArray(value)) return `[${value.map(canonicalize).join(',')}]`;
  if (typeof value === 'object') {
    const keys = Object.keys(value).sort();
    return `{${keys.map((key) => `${JSON.stringify(key)}:${canonicalize(value[key])}`).join(',')}}`;
  }
  return JSON.stringify(null);
}

const secretHex = '000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f';
const secret = Buffer.from(secretHex, 'hex');
const fingerprint = (v) => createHmac('sha256', secret).update(canonicalize(v)).digest('hex');

// Inputs as JSON text: both sides parse the same bytes (JSON.parse / serde_json).
const texts = [
  'null', 'true', 'false', '0', '-0', '1', '1.0', '-1.5', '0.1', '0.2', '0.30000000000000004',
  '100', '1e21', '1e20', '123456789012345680000', '1e-6', '1e-7', '0.000001', '0.0000001', '1.5e-7',
  '123e-20', '5e-324', '1.7976931348623157e308', '9007199254740993', '9007199254740992',
  '12345678901234567890', '18446744073709551616', '-9223372036854775808', '4.35', '2.5e+25', '1E3',
  '0.1e1', '1234.5678', '-0.0', '3.141592653589793', '1e100', '6.02214076e23', '255e-3',
  '""', '"plain"', '"quote\\" backslash\\\\ slash\\/"', '"\\b\\f\\n\\r\\t"',
  '"\\u0000\\u0001\\u001f\\u007f\\u0080"', '"\\u2028\\u2029"', '"emoji 😀 and é and 中文"', '"\\ud83d\\ude00"',
  '"\\uFEFF\\uFFFF"', '[]', '{}', '[1,[2,[3,{}]],"x",null]',
  '{"b":1,"a":2,"c":{"z":[{"y":1,"x":2}],"a":null}}',
  '{"10":1,"9":2,"a":3,"B":4,"_":5,"":6}',
  '{"\\uff61":1,"😀":2,"\\ue000":3,"z":4,"é":5}',
  '{"__proto__":{"x":1},"constructor":2}',
  '{"a":1,"a":2}',
  '{"tool":"computer_act","args":{"task_ref":"task_0123","action":{"kind":"type","text":"Hello, world!\\n"},"expect":[{"kind":"window","title":"Mousepad"}],"timeout_ms":1500.0}}',
  '{"k\\u0007":"\\u000b","nested":{"deep":{"deeper":[0.5,-0,1e21,1e-7]}}}',
];

const vectors = texts.map((text) => {
  const value = JSON.parse(text);
  return { text, canonical: canonicalize(value), fingerprint: fingerprint(value) };
});

// Values that cannot be written as JSON text. `rust` is the equivalent Rust input.
const special = [
  { rust: '{"a":null,"b":1}', value: { a: undefined, b: 1 } },
  { rust: '[null,null]', value: [undefined, () => 1] },
  { rust: '{"outcome":"completed","summary":null,"criteria":null,"artifact_refs":null}',
    value: { outcome: 'completed', summary: undefined, criteria: undefined, artifact_refs: undefined } },
];
for (const item of special) {
  vectors.push({ text: item.rust, canonical: canonicalize(item.value), fingerprint: fingerprint(item.value), from_undefined: true });
}

console.log(JSON.stringify({ secret_hex: secretHex, vectors }, null, 1));
