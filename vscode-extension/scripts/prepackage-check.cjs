const fs = require('node:fs');
const path = require('node:path');

const root = path.resolve(__dirname, '..');
const packageJsonPath = path.join(root, 'package.json');
const errors = [];

function fail(message) {
  errors.push(message);
}

function requireFile(relativePath, options = {}) {
  const absolutePath = path.join(root, relativePath);
  let stat;
  try {
    stat = fs.statSync(absolutePath);
  } catch (error) {
    fail(`${relativePath} is missing`);
    return;
  }

  if (!stat.isFile()) {
    fail(`${relativePath} is not a file`);
    return;
  }

  if (options.executable && (stat.mode & 0o111) === 0) {
    fail(`${relativePath} must be executable`);
  }

  if (options.nonEmpty && stat.size === 0) {
    fail(`${relativePath} must not be empty`);
  }
}

function requireDirectory(relativePath) {
  const absolutePath = path.join(root, relativePath);
  try {
    if (!fs.statSync(absolutePath).isDirectory()) {
      fail(`${relativePath} is not a directory`);
    }
  } catch (error) {
    fail(`${relativePath} is missing`);
  }
}

let packageJson;
try {
  packageJson = JSON.parse(fs.readFileSync(packageJsonPath, 'utf8'));
} catch (error) {
  fail(`package.json could not be read: ${error.message}`);
}

if (path.basename(root) !== 'vscode-extension') {
  fail(`script must run from the vscode-extension package root, got ${root}`);
}

if (packageJson && packageJson.name !== 'himalaya-code-vscode') {
  fail(`unexpected package name: ${packageJson.name}`);
}

if (packageJson && packageJson.main !== './out/extension.js') {
  fail(`package.json main must be ./out/extension.js, got ${packageJson.main}`);
}

requireDirectory('out');
requireFile('out/extension.js', { nonEmpty: true });
requireFile('out/chatPanel.js', { nonEmpty: true });
requireFile('out/streamProtocol.js', { nonEmpty: true });
requireDirectory('bin');
requireFile('bin/Himalaya-linux-x64', { executable: true, nonEmpty: true });
requireFile('bin/skills/document-generator/SKILL.md', { nonEmpty: true });
requireFile('bin/skills/document-generator/references/document-spec.md', { nonEmpty: true });
requireFile('bin/skills/document-generator/scripts/chart_asset.py', { executable: true, nonEmpty: true });
requireDirectory('bin/python');
requireFile('bin/python/run_doc_service.py', { executable: true, nonEmpty: true });
requireFile('bin/python/himalaya_doc_service/pyproject.toml', { nonEmpty: true });
requireFile('bin/python/himalaya_doc_service/src/himalaya_doc_service/server.py', { nonEmpty: true });
requireFile('bin/python/himalaya_doc_service/src/himalaya_doc_service/tools/extract.py', { nonEmpty: true });
requireFile('bin/python/himalaya_doc_service/src/himalaya_doc_service/tools/pptx_gen.py', { nonEmpty: true });
requireDirectory('bin/python/wheelhouse');
requireFile('bin/python/wheelhouse/himalaya_doc_service-0.1.0-py3-none-any.whl', { nonEmpty: true });
requireFile('bin/python/wheelhouse/pymupdf-1.27.2.3-cp310-abi3-manylinux_2_28_x86_64.whl', { nonEmpty: true });
requireFile('bin/python/wheelhouse/pdfplumber-0.11.10-py3-none-any.whl', { nonEmpty: true });
requireFile('bin/python/wheelhouse/python_pptx-1.0.2-py3-none-any.whl', { nonEmpty: true });
requireFile('bin/python/wheelhouse/matplotlib-3.10.9-cp310-cp310-manylinux2014_x86_64.manylinux_2_17_x86_64.whl', { nonEmpty: true });
requireDirectory('media');
requireFile('media/himalaya.svg', { nonEmpty: true });
requireFile('LICENSE', { nonEmpty: true });
requireFile('.vscodeignore', { nonEmpty: true });

if (errors.length > 0) {
  console.error('VSIX prepackage checks failed:');
  for (const error of errors) {
    console.error(`- ${error}`);
  }
  process.exit(1);
}

console.log('VSIX prepackage checks passed.');
