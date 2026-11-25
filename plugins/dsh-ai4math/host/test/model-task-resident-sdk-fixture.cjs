// Synthetic SDK surface for resident process/CLI tests; never performs I/O to a model.
const assert = require('node:assert/strict');
const { writeFileSync } = require('node:fs');
const { join } = require('node:path');

exports.Context = class {
  constructor() {
    const registered = [];
    this.tools = { definitions: () => registered };
    this.agentPresets = {
      register: async () => () => {}, mount: async () => {}, composedPreset: () => 'ai4math-assignment',
    };
    this.llm = { listProviders: () => [], registerAdapter: () => () => {} };
    this.agents = { create: async ({ setup, signal }) => {
      const ctx = { tools: { restrict() {}, guard() {}, setExecutor() {}, register(tool) { registered.push(tool); } } };
      const agent = {
        ctx, cancel() {}, followup() {}, session: { snapshotEvents: () => [{ type: 'turn/end', data: { reason: { kind: 'completed' } } }] },
        async whenIdle() {
          const context = registered.find((tool) => tool.name === 'ai4math_task_context');
          const { task_id: id } = context.execute({}, { agent, signal });
          if (id === 'hung-worker') {
            writeFileSync(join(__dirname, 'worker-pid'), String(process.pid));
            await new Promise(() => setInterval(() => {}, 1_000));
          }
          const submit = registered.find((tool) => tool.name === 'ai4math_task_submit');
          submit.execute({ proof_body: 'by trivial' }, { agent, signal });
        },
      };
      await setup(ctx, agent);
      return { agent, async dispose() {} };
    } };
  }
  get() {}
  plugin() { return { async dispose() {} }; }
};
exports.LlmAdapter = class {};
exports.resolveRetryPolicy = () => ({});
exports.createUserMessage = (message) => message;
exports.NativeToolExecutor = class {};
exports.DeepSeekAdapter = class {
  constructor({ options, resolveAuth }) {
    assert.equal(resolveAuth().headers['x-api-key'], 'fixture-resident-key-local');
    assert.equal(process.env.AI4MATH_RESIDENT_KEY, undefined);
    assert.ok(!process.argv.join(' ').includes('fixture-resident-key-local'));
    assert.equal(options().baseURL, 'http://127.0.0.1:65535');
  }
  prepareCall() { throw new Error('synthetic SDK does not call a model'); }
};
exports.resolveAdapterOptions = (options) => options;
exports.default = () => {};
