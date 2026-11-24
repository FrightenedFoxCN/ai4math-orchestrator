// Trusted operator entry point; not a Cordis plugin or a sandbox for adapter code.
import { createRequire } from 'node:module';
import { isAbsolute, join, relative, resolve, sep } from 'node:path';
import { performance } from 'node:perf_hooks';
import { pathToFileURL } from 'node:url';
import { installModelTaskTools } from './model-task.mjs';

const preset = 'ai4math-assignment';
const sessionId = 'ai4math-task'; // Identifies an ephemeral session in a per-call registry only.
const maxToolCalls = 8;
const maxTokens = 4096;
const messages = {
  invalid: 'Invalid SDK model task options.',
  unavailable: 'The supplied SDK cannot provision a restricted model task.',
  cancelled: 'The SDK model task was cancelled.',
  timeout: 'The SDK model task exceeded its deadline.',
  budget: 'The SDK model task exceeded its call budget.',
  denied: 'The SDK model task request surface did not match its assignment.',
  incomplete: 'The SDK model task turn did not complete.',
  assignment: 'The SDK model task did not supply an admissible suggestion.',
  cleanup: 'The SDK model task did not dispose cleanly.',
};
const ownFailures = new WeakSet();
function failure(code) {
  const error = Object.assign(new Error(messages[code]), { name: 'Ai4mathModelTaskRunError', code });
  ownFailures.add(error);
  return error;
}
const routePart = (part) => typeof part === 'string' && part.length <= 200 &&
  /^[A-Za-z0-9][A-Za-z0-9._:/-]*$/u.test(part) && !/[\r\n]/u.test(part);

/** Run one turn, with no automatic retry, returning only its unverified tool submission.
 * A trusted in-process adapter instance, or a synchronous factory
 * registerAdapter({ LlmAdapter }) returning one, is required. The runner owns
 * registration. The adapter and SDK path are operator trust boundaries, not
 * model-controlled data. Adapter preparation, streaming and iterator cleanup
 * must cooperate with the SDK signal and must not start their own retries.
 * Cancellation revokes the tools, cancels the SDK turn and waits for it to
 * drain before disposal. It cannot preempt arbitrary JS or a stuck adapter.
 */
export async function runModelTaskAssignment({ dshRoot, provider, model, adapter, registerAdapter,
  packet, signal, maxModelCalls = 4 } = {}) {
  const start = performance.now();
  if (typeof dshRoot !== 'string' || dshRoot.length > 4096 || !isAbsolute(dshRoot) || dshRoot.includes('\0') ||
      !routePart(provider) || !routePart(model) || !(signal instanceof AbortSignal) ||
      !Number.isSafeInteger(maxModelCalls) || maxModelCalls < 1 || maxModelCalls > 8 ||
      (adapter !== undefined) === (registerAdapter !== undefined) ||
      (registerAdapter !== undefined && typeof registerAdapter !== 'function') ||
      !packet || ![Object.prototype, null].includes(Object.getPrototypeOf(packet))) throw failure('invalid');
  // Detach the packet before any await, without invoking accessors. The scoped
  // tool installer still validates the complete packet before any model call.
  packet = Object.create(null, Object.getOwnPropertyDescriptors(packet));
  const wallSeconds = Object.getOwnPropertyDescriptor(packet, 'max_wall_seconds')?.value;
  if (!Number.isInteger(wallSeconds) || wallSeconds < 1 || wallSeconds > 60) throw failure('invalid');
  if (signal.aborted) throw failure('cancelled');

  const deadline = start + wallSeconds * 1000;
  const lifetime = new AbortController();
  let stopped, agent, assignment, assigned, unregisterAdapter, unregisterPreset, result;
  let modelCalls = 0, toolCalls = 0, stage = 'unavailable';
  const fibers = [];
  function stop(code) {
    if (stopped) return stopped;
    stopped = failure(code);
    assignment?.revoke();
    lifetime.abort(stopped);
    // Agent creation's signal does not cancel a published agent's turn.
    // The installed Agent.cancel accepts a closed cause union, not an Error.
    try { agent?.cancel({ kind: 'parent' }); }
    catch { /* The SDK aborts in cancel's finally; disposal is still attempted. */ }
    return stopped;
  }
  function assertLive() {
    if (signal.aborted) throw stop('cancelled');
    if (performance.now() >= deadline) throw stop('timeout');
    if (stopped) throw stopped;
  }
  const onAbort = () => stop('cancelled');
  const timer = setTimeout(() => stop('timeout'), Math.max(0, deadline - performance.now()));
  signal.addEventListener('abort', onAbort, { once: true });

  let error, cleanupError;
  try {
    assertLive();
    const root = resolve(dshRoot);
    const manifest = join(root, 'package.json');
    const lookup = createRequire(manifest);
    const sdk = (name) => {
      const entry = lookup.resolve('@deepseek-ai/' + name);
      const ownedPath = relative(root, entry);
      if (!ownedPath || ownedPath === '..' || ownedPath.startsWith('..' + sep) || isAbsolute(ownedPath)) {
        throw failure('unavailable'); // No ancestor/global/NODE_PATH SDK fallback.
      }
      return lookup(entry);
    };
    const { Context } = sdk('cordis');
    const { LlmAdapter, createUserMessage, resolveRetryPolicy } = sdk('dsh-llm');
    const { NativeToolExecutor } = sdk('dsh-tool-executor-native');
    const ctx = new Context();
    async function mount(name, config) {
      assertLive();
      const fiber = ctx.plugin(sdk(name).default, config);
      fibers.unshift(fiber);
      await fiber;
      assertLive();
    }
    await mount('cordis-plugin-loader', { baseUrl: pathToFileURL(manifest).href });
    for (const name of ['dsh-session', 'dsh-session-projection', 'dsh-agent', 'dsh-llm',
      'dsh-system-prompt', 'dsh-tools', 'dsh-agent-loop']) await mount(name);
    await mount('dsh-agent-preset-registry', { default: preset });
    unregisterPreset = await ctx.agentPresets.register({ id: preset, plugins: [] });
    if (['credentials', 'settings', 'sessionPersistence'].some((name) => ctx.get(name) !== undefined) ||
        ctx.llm.listProviders().length !== 0) throw failure('unavailable');
    assertLive();
    const transport = registerAdapter ? registerAdapter(Object.freeze({ LlmAdapter })) : adapter;
    if (!transport || typeof transport.prepareCall !== 'function') throw failure('invalid');
    const noRetries = resolveRetryPolicy({ mode: 'normal', maxRetries: 0 }, 'ai4math assignment');

    // The SDK prepares a route and then projects the actual request (including
    // tool updates) before invoking the prepared stream. Auditing the request
    // here checks the final SDK envelope, not just the tool registry.
    class AssignmentAdapter extends LlmAdapter {
      providerRetryPolicy() { return noRetries; }
      async prepareCall(requestProvider, requestModel, sdkSignal) {
        assertLive();
        if (requestProvider !== provider || requestModel !== model || !(sdkSignal instanceof AbortSignal)) {
          throw stop('denied');
        }
        sdkSignal.throwIfAborted();
        if (++modelCalls > maxModelCalls) throw stop('budget');
        const prepared = await transport.prepareCall(provider, model, sdkSignal);
        assertLive();
        if (!prepared || typeof prepared.stream !== 'function') throw stop('incomplete');
        return { model: prepared.model, stream: (request) => streamPrepared(prepared, request, sdkSignal) };
      }
    }
    async function* streamPrepared(prepared, request, sdkSignal) {
      assertLive();
      if (request.provider !== provider || request.model !== model || request.signal !== sdkSignal) throw stop('denied');
      try {
        assignment.assertVisibleTools(request.tools?.map((tool) => tool.name));
      } catch { throw stop('denied'); }
      let finished = false;
      for await (const chunk of prepared.stream(request)) {
        assertLive();
        sdkSignal.throwIfAborted();
        if (chunk?.type === 'finish') finished = true;
        yield chunk;
      }
      // The SDK otherwise defaults omitted finish chunks to stop, which is
      // insufficient evidence for accepting an earlier step's submission.
      if (!finished) throw stop('incomplete');
    }
    // Unlike a request guard, the executor budget counts every attempted call,
    // including names that are not installed and are rejected by the guard.
    class AssignmentExecutor extends NativeToolExecutor {
      execute(input) {
        assertLive();
        toolCalls += input.calls.length;
        if (toolCalls > maxToolCalls) throw stop('budget');
        return super.execute(input);
      }
    }

    unregisterAdapter = ctx.llm.registerAdapter([provider], new AssignmentAdapter());
    assigned = await ctx.agents.create({
      sessionId, signal: lifetime.signal,
      meta: { agentPreset: preset },
      agentOptions: { provider, model, maxTokens },
      async setup(agentCtx, taskAgent) {
        await ctx.agentPresets.mount(agentCtx, preset);
        assertLive();
        agentCtx.tools.setExecutor(new AssignmentExecutor(agentCtx, () => 1));
        try { assignment = installModelTaskTools(agentCtx.tools, taskAgent, packet, lifetime.signal); }
        catch (error) { if (error?.code === 'invalid') throw failure('invalid'); throw error; }
      },
    });
    agent = assigned.agent;
    if (typeof agent.cancel !== 'function' || typeof agent.whenIdle !== 'function' ||
        typeof assigned.dispose !== 'function' || typeof agent.session?.snapshotEvents !== 'function' ||
        ctx.agentPresets.composedPreset(agent.ctx) !== preset) throw failure('unavailable');
    assertLive();
    try { assignment.assertVisibleTools(ctx.tools.definitions(agent).map((tool) => tool.name)); }
    catch { throw failure('denied'); }
    stage = 'incomplete';
    agent.followup(createUserMessage({
      content: [{ type: 'text', text: 'Read the assigned task with ai4math_task_context and submit one advisory suggestion with ai4math_task_submit. Then finish the turn.' }],
      source: { kind: 'user' },
    }));
    await agent.whenIdle();
    assertLive();
    const ends = agent.session.snapshotEvents().filter((event) => event.type === 'turn/end');
    if (ends.length !== 1 || ends[0].data.reason.kind !== 'completed') throw failure('incomplete');
    result = assignment.takeResult();
    if (!result) throw failure('assignment');
  } catch (caught) {
    error = stopped ?? (ownFailures.has(caught) ? caught : failure(stage));
  } finally {
    assignment?.revoke();
    const cleanup = async (dispose) => {
      try { await dispose(); }
      catch { cleanupError ??= failure('cleanup'); }
    };
    // The SDK disposer cancels and drains too, including failure paths after
    // followup but before whenIdle. Keep services and the route alive for it.
    agent = undefined;
    if (assigned) await cleanup(() => assigned.dispose());
    if (unregisterAdapter) await cleanup(unregisterAdapter);
    if (unregisterPreset) await cleanup(unregisterPreset);
    for (const fiber of fibers) await cleanup(() => fiber.dispose());
    signal.removeEventListener('abort', onAbort);
    clearTimeout(timer);
  }
  assertLive();
  if (error || cleanupError) throw error ?? cleanupError;
  return result;
}
