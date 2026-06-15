const fs = require('node:fs');
const path = require('node:path');

const root = path.resolve(__dirname, '..');
const source = path.resolve(root, '..', 'rust', 'crates', 'commands', 'bundled', 'skills');
const target = path.join(root, 'bin', 'skills');

if (!fs.existsSync(source)) {
  throw new Error(`Bundled skills source is missing: ${source}`);
}

fs.rmSync(target, { recursive: true, force: true });
fs.mkdirSync(path.dirname(target), { recursive: true });
fs.cpSync(source, target, { recursive: true });
console.log(`Staged bundled skills: ${target}`);
