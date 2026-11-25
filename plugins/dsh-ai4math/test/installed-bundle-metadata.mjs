// Offline metadata check in the installed DSH Node runtime. Does not load the plugin or a profile.
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';
import { isAbsolute, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const installDir = process.argv[2];
assert.ok(installDir && isAbsolute(installDir), 'pass the absolute directory of the installed @deepseek-ai/dsh package');
const sdk = createRequire(join(installDir, 'package.json'));
const cli = sdk('@deepseek-ai/dsh/package.json');
assert.equal(cli.name, '@deepseek-ai/dsh');
assert.equal(cli.version, '0.2.2-rc.5');
const {
  bundlePatchPaths, composeEntries, evaluatePluginCompatibility, loadOverlayPatches, readPluginMeta, readProfileManifest,
} = sdk('@deepseek-ai/dsh-app-boot');
const root = fileURLToPath(new URL('../', import.meta.url));
const manifest = readProfileManifest('dsh', root);
assert.equal(evaluatePluginCompatibility(manifest), undefined);
const target = evaluatePluginCompatibility(manifest, {}, '0.2.2');
assert.deepEqual(target.requirements, { 'engines.dsh': '0.2.2-rc.5' });
assert.equal(target.exempted, false);
const patches = bundlePatchPaths(root, manifest.dsh.bundle).flatMap((file) => loadOverlayPatches('dsh', file));
const composed = composeEntries([patches]);
assert.equal(composed.length, 2);
const presetRow = composed.find((row) => row.id === 'preset-ai4math-chief');
assert.equal(presetRow?.name, '@deepseek-ai/dsh-agent-preset');
assert.equal(presetRow?.config?.id, 'ai4math-chief');
assert.ok(Array.isArray(presetRow?.config?.plugins) && presetRow.config.plugins.length > 0);
const pluginRow = composed.find((row) => row.id === 'dsh-ai4math');
assert.deepEqual(pluginRow, {
  id: 'dsh-ai4math', name: 'dsh-ai4math', disabled: true,
  config: { rolePresets: { chief: 'ai4math-chief' }, proofSlots: {} },
});
const meta = readPluginMeta(manifest.name, new URL('../package.json', import.meta.url).href);
assert.equal(meta.error, undefined);
for (const language of ['en', 'zh']) {
  const expected = JSON.parse(readFileSync(join(root, 'locale', `${language}.json`), 'utf8')).meta;
  assert.equal(meta.title[language], expected.title);
  assert.equal(meta.description[language], expected.description);
}
assert.equal(meta.icon, 'data:image/svg+xml;base64,' + readFileSync(join(root, 'icon.svg')).toString('base64'));
console.log('DSH 0.2.2-rc.5: manifest, bilingual metadata, icon, built-in ai4math-chief preset, and default-disabled dsh-ai4math row accepted');
