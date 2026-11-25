import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { createRequire } from 'node:module';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';

const root = new URL('../', import.meta.url);
const pathOf = (name) => fileURLToPath(new URL(name, root));
const json = (name) => JSON.parse(readFileSync(pathOf(name), 'utf8'));
const pkg = json('package.json');
const resolve = createRequire(pathOf('package.json')).resolve;
const exports = {
  '.': 'host/index.mjs',
  './client': 'client/client.js',
  './package.json': 'package.json',
  './icon': 'icon.svg',
  './locale/en.json': 'locale/en.json',
  './locale/zh.json': 'locale/zh.json',
};

test('bundle manifest declares the exact DSH target and resolves its display resources', () => {
  assert.equal(pkg.name, 'dsh-ai4math');
  assert.equal(pkg.engines.dsh, '0.2.2-rc.5');
  assert.deepEqual(pkg.dsh.bundle, { patch: './cordis.patch.yml' });
  assert.equal(pkg.dsh.client.platform, 'web');
  for (const [subpath, target] of Object.entries(exports)) {
    const specifier = subpath === '.' ? pkg.name : pkg.name + subpath.slice(1);
    assert.equal(resolve(specifier), pathOf(target));
  }
  for (const language of ['en', 'zh']) {
    const { meta } = json(`locale/${language}.json`);
    assert.ok(meta.title.trim());
    assert.ok(meta.description.trim());
    assert.equal(meta.detail, undefined, 'use the bilingual package README as the fallback');
  }
});

test('dry-run pack includes runtime and display files but excludes all test fixtures', (t) => {
  const npm = process.env.npm_execpath;
  const scratch = mkdtempSync(join(tmpdir(), 'ai4math-pack-'));
  t.after(() => rmSync(scratch, { recursive: true }));
  const args = [
    'pack', '--dry-run', '--json', '--ignore-scripts', '--offline', '--logs-max=0',
    '--cache=' + join(scratch, 'cache'),
    '--userconfig=' + join(scratch, 'user.npmrc'),
    '--globalconfig=' + join(scratch, 'global.npmrc'),
  ];
  const result = npm?.endsWith('npm-cli.js')
    ? execFileSync(process.execPath, [npm, ...args], { cwd: root, encoding: 'utf8' })
    : execFileSync('npm', args, { cwd: root, encoding: 'utf8' });
  const [packed] = JSON.parse(result);
  const files = packed.files.map((entry) => entry.path);
  const host = readdirSync(pathOf('host')).filter((name) => name.endsWith('.mjs')).map((name) => `host/${name}`);
  const client = readdirSync(pathOf('client')).filter((name) => /\.(mjs|cjs|js|css)$/.test(name)).map((name) => `client/${name}`);
  assert.deepEqual(files.sort(), [...new Set([
    'README.md', 'cordis.patch.yml', ...Object.values(exports), ...host, ...client,
  ])].sort());
});
