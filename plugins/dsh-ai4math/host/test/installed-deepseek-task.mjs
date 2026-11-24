// Run from the repository root; use only a local HTTP fixture and synthetic keys:
// env -i PATH=/usr/bin:/bin ELECTRON_RUN_AS_NODE=1 \
//   '/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness' \
//   plugins/dsh-ai4math/host/test/installed-deepseek-task.mjs \
//   '/Applications/DeepSeek Harness.app/Contents/Resources/app.asar/dsh' "$PWD/target/debug/ai4math"
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { once } from 'node:events';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { isAbsolute, join } from 'node:path';
import { runDeepSeekModelTask } from '../model-task-deepseek.mjs';

const [dshRoot, cliPath, ...extra] = process.argv.slice(2);
assert.ok(dshRoot && cliPath && isAbsolute(dshRoot) && isAbsolute(cliPath) && extra.length === 0,
  'usage: installed-deepseek-task.mjs ABSOLUTE_DSH_SDK_ROOT ABSOLUTE_BUILT_AI4MATH');

const model = 'fixture-math-model';
const apiKey = 'fixture-math-key-local-only';
const wrongKey = 'fixture-math-wrong-local-only';
const claimId = 'fixture-claim';
const modulePath = 'Proof.lean';
const decl = 'goal';
const formalHeader = 'theorem goal : True';
const wallSeconds = 15;
const privateMarker = 'WRITER_PRIVATE_HTTP_FIXTURE_CONTEXT';
const contextTool = 'ai4math_task_context';
const submitTool = 'ai4math_task_submit';
const contextCall = 'toolu_fixture_context';
const submitCall = 'toolu_fixture_submit';
const commandEnv = { PATH: '/usr/bin:/bin', GIT_CONFIG_NOSYSTEM: '1', GIT_CONFIG_GLOBAL: '/dev/null' };
const textSchema = { type: 'string', minLength: 1, maxLength: 8192 };

function schema(properties) {
  return { type: 'object', additionalProperties: false, properties, required: Object.keys(properties) };
}

function expectedTools(role) {
  const proving = role === 'prover_worker';
  return [
    {
      name: contextTool,
      description: 'Read only the current assigned Lean task, not project history.',
      input_schema: schema({}),
    },
    {
      name: submitTool,
      description: proving
        ? 'Submit one unverified Lean proof-body suggestion, never a proof receipt or promotion.'
        : 'Submit one advisory independent review verdict, never a proof receipt or promotion.',
      input_schema: proving ? schema({ proof_body: textSchema }) : schema({
        verdict: { type: 'string', enum: ['looks_consistent', 'needs_changes', 'insufficient_context'] },
        rationale: textSchema,
      }),
    },
  ];
}

function run(exe, args, cwd, { absent = false } = {}) {
  const result = spawnSync(exe, args, {
    cwd, env: commandEnv, timeout: 15_000, maxBuffer: 64 * 1024,
    stdio: ['ignore', 'pipe', 'pipe'], encoding: 'utf8',
  });
  if (result.error) throw result.error;
  if (absent) {
    assert.equal(result.status, 1);
    assert.equal(result.stdout.trim(), '');
    assert.match(result.stderr, /no retained suggestion for this task/);
  } else assert.equal(result.status, 0, 'Fixture command failed: ' + args[0] + '\n' + result.stderr);
  return result.stdout.trim();
}

function sendHttpError(response, status, type) {
  response.writeHead(status, { 'content-type': 'application/json' });
  response.end(JSON.stringify({ type: 'error', error: { type, message: 'fixture-only diagnostic' } }));
}

function sendStream(response, call) {
  function event(type, rest = {}) {
    response.write('event: ' + type + '\ndata: ' + JSON.stringify({ type, ...rest }) + '\n\n');
  }
  response.writeHead(200, { 'content-type': 'text/event-stream' });
  event('message_start', { message: {
    id: 'msg_fixture', type: 'message', role: 'assistant', model, content: [],
    stop_reason: null, stop_sequence: null, usage: { input_tokens: 12, output_tokens: 0 },
  } });
  if (call) {
    const { id, name, input } = call;
    event('content_block_start', { index: 0, content_block: { type: 'tool_use', id, name, input: {} } });
    // Deliver arguments as multiple partial JSON events through the real SDK parser.
    const json = JSON.stringify(input);
    for (const partial_json of [json.slice(0, 1), json.slice(1)]) {
      event('content_block_delta', { index: 0, delta: { type: 'input_json_delta', partial_json } });
    }
  } else {
    event('content_block_start', { index: 0, content_block: { type: 'text', text: '' } });
    event('content_block_delta', { index: 0, delta: { type: 'text_delta', text: 'Fixture turn complete.' } });
  }
  event('content_block_stop', { index: 0 });
  event('message_delta', { delta: { stop_reason: call ? 'tool_use' : 'end_turn', stop_sequence: null },
    usage: { output_tokens: 10 } });
  event('message_stop');
  response.end();
}

function assertToolHistory(messages, expected) {
  const uses = [], results = [];
  for (const { role, content } of messages) {
    for (const block of content) {
      if (block.type === 'tool_use') {
        assert.equal(role, 'assistant');
        uses.push(block);
      } else if (block.type === 'tool_result') {
        assert.equal(role, 'user');
        assert.notEqual(block.is_error, true);
        assert.equal(block.content.length, 1);
        assert.equal(block.content[0].type, 'text');
        results.push({ id: block.tool_use_id, value: JSON.parse(block.content[0].text) });
      }
    }
  }
  assert.deepEqual(uses, expected.map(({ call }) => ({ type: 'tool_use', ...call })));
  assert.deepEqual(results, expected.map(({ call, value }) => ({ id: call.id, value })));
}

const dir = await mkdtemp(join(tmpdir(), 'ai4math-installed-deepseek-'));
const root = join(dir, 'project');
const source = join(dir, 'source');
const bareRepo = join(dir, 'repo.git');
const realFetch = globalThis.fetch;
let scenario;
const server = createServer(async (request, response) => {
  try {
    assert.ok(scenario, 'unexpected fixture HTTP request');
    assert.equal(request.url, '/v1/messages', 'the adapter must not follow the local 307 redirect');
    const index = scenario.requests++;
    assert.ok(index < scenario.expectedRequests, 'unexpected extra model request');
    assert.equal(request.method, 'POST');
    assert.equal(request.headers.authorization, undefined);
    assert.equal(request.headers.cookie, undefined);
    assert.equal(request.headers['x-api-key'], scenario.key);
    assert.equal(request.headers['anthropic-version'], '2023-06-01');
    assert.equal(request.headers.accept, 'text/event-stream');
    assert.equal(request.headers['content-type'], 'application/json');
    const chunks = [];
    let bytes = 0;
    for await (const chunk of request) {
      bytes += chunk.length;
      assert.ok(bytes <= 64 * 1024, 'oversized fixture request');
      chunks.push(chunk);
    }
    const wire = Buffer.concat(chunks).toString('utf8');
    for (const marker of [privateMarker, 'privateFixture', dir, cliPath, dshRoot, apiKey, wrongKey]) {
      assert.ok(!wire.includes(marker), 'non-assignment data reached the HTTP request body');
    }
    const body = JSON.parse(wire);
    assert.equal(body.model, model);
    assert.equal(body.stream, true);
    assert.deepEqual(body.tools.toSorted((a, b) => a.name.localeCompare(b.name)), expectedTools(scenario.packet.role));
    assertToolHistory(body.messages, scenario.history);
    if (scenario.key !== apiKey) {
      sendHttpError(response, 401, 'authentication_error');
    } else if (scenario.fail === 'redirect') {
      response.writeHead(307, { location: '/redirected' });
      response.end();
    } else if (index === 0) {
      const call = { id: contextCall, name: contextTool, input: {} };
      scenario.history.push({ call, value: scenario.packet });
      sendStream(response, call);
    } else if (index === 1) {
      const call = { id: submitCall, name: submitTool, input: scenario.input };
      scenario.history.push({ call, value: { accepted: true, verified: false } });
      sendStream(response, call);
    } else if (scenario.fail === 'final') {
      sendHttpError(response, 503, 'overloaded_error');
    } else {
      sendStream(response);
    }
  } catch (error) {
    if (scenario) scenario.failure ??= error;
    sendHttpError(response, 500, 'api_error');
  }
});

try {
  const git = (cwd, ...args) => run('/usr/bin/git', [
    '-c', 'user.name=Fixture', '-c', 'user.email=fixture@localhost', '-c', 'commit.gpgSign=false',
    '-c', 'core.hooksPath=/dev/null', ...args,
  ], cwd);
  const ai = (command, ...args) => run(cliPath, [command, root, ...args], dir);
  await mkdir(source);
  git(source, 'init', '--initial-branch=main', '--quiet');
  await writeFile(join(source, modulePath), '-- ' + privateMarker + '\ndef privateFixture : Nat := 0\n' +
    formalHeader + ' := {{PROOF_BODY}}\n');
  git(source, 'add', '--', modulePath);
  git(source, 'commit', '--quiet', '-m', 'Freeze local HTTP fixture proof slot');
  git(dir, 'clone', '--local', '--bare', '--quiet', source, bareRepo);
  const base = git(bareRepo, 'rev-parse', 'refs/heads/main');
  ai('init', 'lean4');
  ai('declare-claim', claimId, modulePath, decl, 'Truth fixture', 'True');
  ai('task-bind', bareRepo, modulePath, base);

  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const baseUrl = 'http://127.0.0.1:' + server.address().port;
  const endpoint = baseUrl + '/v1/messages';
  // Any unexpected fetch, including a remote fallback, fails without contacting it.
  // Admitted requests still use the real fetch implementation and local HTTP server.
  globalThis.fetch = (resource, init) => {
    try {
      assert.equal(String(resource), endpoint, 'provider fetch escaped the local Messages endpoint');
    } catch (error) {
      if (scenario) scenario.failure ??= error;
      throw error;
    }
    return realFetch(resource, init);
  };

  for (const [taskId, role, suggestion] of [
    ['fixture-prover', 'prover_worker', { kind: 'proof_body', proof_body: 'by trivial' }],
    ['fixture-reviewer', 'independent_reviewer', { kind: 'review', verdict: 'looks_consistent', rationale: 'The header matches the statement.' }],
  ]) {
    ai('task-register', bareRepo, taskId, 'infrastructure', modulePath, decl);
    const input = role === 'prover_worker'
      ? { proof_body: suggestion.proof_body }
      : { verdict: suggestion.verdict, rationale: suggestion.rationale };
    const packet = {
      version: 1, task_id: taskId, claim_id: claimId, role, engine: 'lean4', module_path: modulePath,
      target_decl: decl, base_commit: base, max_wall_seconds: wallSeconds, informal_statement: 'True', formal_header: formalHeader,
    };
    async function runScenario({ fail } = {}) {
      const key = fail === 'auth' ? wrongKey : apiKey;
      const expectedRequests = fail === 'auth' || fail === 'redirect' ? 1 : 3;
      scenario = { packet, input, key, expectedRequests, fail, history: [], requests: 0 };
      const pending = runDeepSeekModelTask({
        cliPath, root, bareRepo, taskId, claimId, role, wallSeconds, dshRoot, model, apiKey: key, baseUrl,
        // Leave room for a retry so the HTTP request counts would detect one.
        maxModelCalls: 4, signal: new AbortController().signal,
      });
      let summary;
      try {
        if (fail) await assert.rejects(pending, { code: 'assignment' });
        else summary = await pending;
      } finally {
        if (scenario.failure) throw scenario.failure;
      }
      assert.equal(scenario.requests, expectedRequests, 'wrong number of HTTP requests');
      return summary;
    }

    if (role === 'prover_worker') {
      // A final HTTP failure must not retain a submission already accepted by the scoped tool.
      for (const fail of ['auth', 'redirect', 'final']) {
        await runScenario({ fail });
        run(cliPath, ['task-result', root, taskId], dir, { absent: true });
      }
    }
    // Reuse the same task id immediately after failures: no retry and no stranded lease.
    assert.deepEqual(await runScenario(), {
      task_id: taskId, base_commit: base, role, source: 'dsh_model', suggestion_recorded: true, proof_verified: false,
    });
    const { candidate, proof_verified } = JSON.parse(ai('task-result', taskId));
    assert.equal(proof_verified, false);
    assert.equal(candidate.task_id, taskId);
    assert.equal(candidate.claim_id, claimId);
    assert.equal(candidate.base_commit, base);
    assert.equal(candidate.role, role);
    assert.equal(candidate.source, 'dsh_model');
    assert.ok(!Object.hasOwn(candidate, 'executable_sha256'));
    assert.deepEqual(JSON.parse(candidate.result_json), suggestion);
    assert.equal(git(bareRepo, 'rev-parse', 'refs/heads/main'), base);
  }
  console.log('Installed DeepSeekAdapter + local Messages SSE + Rust CLI: exact role schemas and tool round trips; SQLite suggestions unverified; main unchanged');
  console.log('HTTP 401, unfollowed 307, and final 503 created no durable result; the same task was reclaimed without retry');
} finally {
  globalThis.fetch = realFetch;
  if (server.listening) {
    const closed = once(server, 'close');
    server.close();
    server.closeAllConnections();
    await closed;
  }
  await rm(dir, { recursive: true, force: true });
}
