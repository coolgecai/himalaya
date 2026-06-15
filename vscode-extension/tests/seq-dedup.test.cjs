const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

test('seq deduper helper exists in source', () => {
  const root = path.resolve(__dirname, '..');
  const src = fs.readFileSync(path.join(root, 'src', 'seqDedup.ts'), 'utf8');
  assert.match(src, /export class SeqDeduper/);
  assert.match(src, /shouldForward\(/);
});
