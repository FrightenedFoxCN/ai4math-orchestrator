import Schema from '@deepseek-ai/schemastery';
import { createScope } from '@deepseek-ai/dsh-scope';
import { RemoteError, TypertRemoteService } from '@deepseek-ai/dsh-typert-protocol';
import { createTypertRemoteInfo } from '@deepseek-ai/dsh-typert-protocol/remote-info';
import { z } from 'zod';
import { AGENT_ROLES, createBridge, methodsForRole, parseParams } from './bridge.mjs';
import { BridgeError } from './connection.mjs';
import { REMOTE_METHODS, REMOTE_NAMESPACE, REMOTE_PACKAGE } from './remote-contract.mjs';
import { createWorkbench } from './workbench.mjs';

const CHIEF_SAFE_TOOLS = Object.freeze([
  'read', 'read_image', 'glob', 'grep', 'subagent', 'subagent_fork', 'send_message', 'list_agents',
  'workflow', 'web_search', 'web_fetch', 'ask_user_question', 'todo_write', 'present', 'skill',
]);
const READ_SAFE_TOOLS = Object.freeze(['read', 'read_image', 'glob', 'grep', 'web_search', 'web_fetch', 'skill']);
const safeToolsForRole = (role) => role === 'chief' || role === 'regional' ? CHIEF_SAFE_TOOLS
  : role === 'explorer' || role === 'builder' ? READ_SAFE_TOOLS : Object.freeze([]);

export const name = 'dsh-ai4math';
export const inject = ['typert', 'typertGateway'];
export const Config = Schema.object({
  socketPath: Schema.string().description('Absolute path to the local ai4mathd Unix socket; falls back to AI4MATH_SOCKET or the active Workbench project.'),
  timeoutMs: Schema.natural().min(100).max(60_000).default(5000).description('Deadline for each local RPC, including connection.'),
  cliPath: Schema.string().description('Optional absolute path to the local ai4math CLI binary; auto-detected from the repo checkout when omitted.'),
  daemonPath: Schema.string().description('Optional absolute path to the local ai4mathd binary; auto-detected from the repo checkout when omitted.'),
  registryPath: Schema.string().description('Optional absolute path to the Workbench project registry JSON file.'),
  defaultChiefPreset: Schema.string().default('ai4math-chief').description('Agent preset ID launched by one-click Chief sessions from the Workbench.'),
  proofSlots: Schema.object({ prover: Schema.array(Schema.string()), automation: Schema.array(Schema.string()) }).default({})
    .description('Exact frozen slot IDs visible to the configured prover or automation preset. Defaults to none.'),
  rolePresets: Schema.object(Object.fromEntries(AGENT_ROLES.map((role) => [role, Schema.string()]))).default({})
    .description('Optional live Agent preset IDs, keyed by ai4math role. Unmapped agents receive no ai4math tools.'),
});

const resultSchema = z.record(z.string(), z.json());
const methods = Object.fromEntries(Object.entries(REMOTE_METHODS).filter(([, endpoint]) => !endpoint.agentOnly).map(([key, endpoint]) => {
  // A strict descriptor compiled against the same validation used by the model tools.
  const input = z.custom((value) => {
    try { parseParams(key, value); return true; } catch { return false; }
  });
  const symbol = REMOTE_PACKAGE + '#' + REMOTE_NAMESPACE + '/' + key;
  const codec = { mode: 'strict', typeSymbol: symbol + ':params', create: () => input, jsonSchema: endpoint.params };
  return [REMOTE_NAMESPACE + '/' + key, {
    service: REMOTE_NAMESPACE,
    description: endpoint.description,
    invocation: { kind: 'direct' },
    parameters: [{ name: 'params', source: 'json', codec, strictCodec: codec }],
    result: { mode: 'strict', typeSymbol: symbol + ':result', create: () => resultSchema, jsonSchema: { type: 'object' } },
    cancellation: { parameter: 'signal' },
    ...(endpoint.stream ? { mode: 'stream' } : {}),
  }];
}));
const info = createTypertRemoteInfo({ package: REMOTE_PACKAGE, methods }, z);
// Register with the calling Cordis fiber, not via the Loader's automatic ./typert face.
const contribution = { package: REMOTE_PACKAGE, face: 'host', schemas: [], invocations: info.descriptors };

function asRemoteError(error) {
  if (!(error instanceof BridgeError)) throw error;
  return new RemoteError('ai4math/' + error.code, error.message, error.rpcCode === undefined ? {} : { rpcCode: error.rpcCode });
}

function requireOwner(ctx) {
  // Gateway injects the authenticated, live Peer into the derived invocation Context.
  // Neither a body field nor a claimed UI label can select the role.
  if (ctx.invocation?.peer?.principal?.kind !== 'owner') throw asRemoteError(new BridgeError('denied'));
}

class Ai4mathRemote extends TypertRemoteService {
  constructor(ctx, owner, workbench) {
    super(ctx, REMOTE_NAMESPACE);
    this.owner = owner;
    this.workbench = workbench;
  }
  async workbenchState(params) { return callWorkbench(this, 'workbenchState', 'state', params); }
  async projectInit(params) { return callWorkbench(this, 'projectInit', 'projectInit', params); }
  async projectSelect(params) { return callWorkbench(this, 'projectSelect', 'projectSelect', params); }
  async projectRemove(params) { return callWorkbench(this, 'projectRemove', 'projectRemove', params); }
  async daemonStart(params) { return callWorkbench(this, 'daemonStart', 'daemonStart', params); }
  async daemonStop(params) { return callWorkbench(this, 'daemonStop', 'daemonStop', params); }
  async promotionApproval(params, signal) { return callOwner(this, 'promotionApproval', 'promotion.approval', params, signal); }
  async promotionExecute(params, signal) { return callOwner(this, 'promotionExecute', 'promotion.execute', params, signal); }
  async promotionRecover(params, signal) { return callOwner(this, 'promotionRecover', 'promotion.recover', params, signal); }
  async status(params, signal) { return callOwner(this, undefined, 'status', params, signal); }
  async briefingPeek(params, signal) { return callOwner(this, undefined, 'briefingPeek', params, signal); }
  async briefingPull(params, signal) { return callOwner(this, undefined, 'briefingPull', params, signal); }
  async briefingGet(params, signal) { return callOwner(this, undefined, 'briefingGet', params, signal); }
  async frontierList(params, signal) { return callOwner(this, undefined, 'frontierList', params, signal); }
  async inboxList(params, signal) { return callOwner(this, undefined, 'inboxList', params, signal); }
  async alertsList(params, signal) { return callOwner(this, undefined, 'alertsList', params, signal); }
  async wikiSearch(params, signal) { return callOwner(this, undefined, 'wikiSearch', params, signal); }
  async wikiCheckGraveyard(params, signal) { return callOwner(this, undefined, 'wikiCheckGraveyard', params, signal); }
  async wikiProvenance(params, signal) { return callOwner(this, undefined, 'wikiProvenance', params, signal); }
  async *subscribe(params, signal) {
    requireOwner(this.ctx);
    try { yield* this.owner.subscribe(params, signal); }
    catch (error) { throw asRemoteError(error); }
  }
}

async function callWorkbench(service, key, handler, params) {
  requireOwner(service.ctx);
  try {
    const args = parseParams(key, params);
    return await service.workbench[handler](args);
  } catch (error) {
    throw asRemoteError(error);
  }
}

async function callOwner(service, key, method, params, signal) {
  requireOwner(service.ctx);
  try {
    if (key !== undefined) params = parseParams(key, params);
    return await service.owner.call(method, params, signal);
  }
  catch (error) { throw asRemoteError(error); }
}

function configuredRoles(rolePresets) {
  const roleByPreset = new Map();
  for (const role of AGENT_ROLES) {
    if (!Object.hasOwn(rolePresets, role)) continue;
    const preset = rolePresets[role];
    if (preset === undefined) continue;
    if (typeof preset !== 'string' || preset.length === 0 || roleByPreset.has(preset)) {
      throw new TypeError('ai4math rolePresets must map distinct nonempty preset IDs.');
    }
    roleByPreset.set(preset, role);
  }
  return roleByPreset;
}

function installTools(ctx, bridge, roleByPreset, proofSlots) {
  if (roleByPreset.size === 0) return;
  ctx.inject(['agentPresets', 'agents', 'tools'], async (scoped) => {
    const live = new Map();
    const pending = new WeakMap();
    let active = true;
    const resolveRole = (agent) => roleByPreset.get(scoped.agentPresets.composedPreset(agent.ctx));
    const release = async (owned) => { owned.controller.abort(); await owned.scope.dispose(); };

    async function sync(agent) {
      if (!active) return;
      const previous = live.get(agent);
      if (previous?.role === resolveRole(agent)) return;
      if (previous) {
        live.delete(agent);
        await release(previous);
      }
      if (!active) return;
      const role = resolveRole(agent);
      if (role === undefined) return;
      const owned = { role, scope: createScope(agent.ctx, agent), controller: new AbortController() };
      live.set(agent, owned);
      const safeNames = new Set(safeToolsForRole(role));
      const visible = typeof agent.ctx.tools.definitions === 'function' ? agent.ctx.tools.definitions(agent) : [];
      const safe = [...new Set(visible.map(tool => tool.name).filter(name => safeNames.has(name)))];
      const registered = new Set();
      owned.scope.ctx.tools.restrict({ allow:safe });
      owned.scope.ctx.tools.guard((execution) => {
        if (execution.agent !== agent || live.get(agent) !== owned || resolveRole(agent) !== role) {
          return 'AI4Math role capability is no longer active.';
        }
        return safeNames.has(execution.name) || registered.has(execution.name)
          ? undefined : 'Tool is outside this AI4Math role capability.';
      });
      owned.scope.ctx.effect(() => () => {
        owned.controller.abort();
        if (live.get(agent) === owned) live.delete(agent);
      }, 'ai4math.agent-tools');
      const slots = proofSlots[role] ?? [];
      const capability = bridge.bind(role,{proofSlots:slots,bindingId:String(agent.id)});
      try {
        for (const key of methodsForRole(role)) {
          const endpoint = REMOTE_METHODS[key];
          if (endpoint.stream || (key === 'proofSubmit' && slots.length === 0)) continue;
          const toolName = 'ai4math_' + endpoint.method.replaceAll('.', '_');
          registered.add(toolName);
          owned.scope.ctx.tools.register({
            name: toolName,
            description: endpoint.description,
            parameters: endpoint.params,
            output: { schema: { type: 'object' }, render: (_args, value) => [{ type: 'text', text: JSON.stringify(value) }] },
            async execute(args, execution) {
              // The scope controls visibility; check the live execution identity again at dispatch.
              const bound = () => active && execution.agent === agent && live.get(agent) === owned && resolveRole(agent) === role;
              if (!bound()) throw new BridgeError('denied');
              const signal = AbortSignal.any([execution.signal, owned.controller.signal]);
              const result = await capability.call(key, args, signal);
              if (!bound() || signal.aborted) throw new BridgeError('cancelled');
              return result;
            },
          });
        }
      } catch (error) {
        if (live.get(agent) === owned) live.delete(agent);
        await release(owned);
        throw error;
      }
    }

    function schedule(agent) {
      const before = pending.get(agent) ?? Promise.resolve();
      const next = before.catch(() => {}).then(() => sync(agent));
      pending.set(agent, next);
      return next;
    }
    scoped.effect(() => async () => {
      active = false;
      await Promise.all([...live.values()].map(release));
      live.clear();
    }, 'ai4math.agent-tools.lifecycle');
    scoped.on('agent/created', ({ agent }) => schedule(agent));
    scoped.on('agent-preset/selected', (id) => {
      const agent = scoped.agents.get(id);
      if (agent !== undefined) void schedule(agent).catch(() => scoped.logger.warn('Could not refresh scoped ai4math tools.'));
    });
    await Promise.all(scoped.agents.list().map(schedule));
  });
}

export function apply(ctx, config = {}) {
  const roleByPreset = configuredRoles(config.rolePresets ?? {});
  const proofSlots = Object.fromEntries(['prover','automation'].map(role => {
    const ids = config.proofSlots?.[role] ?? [];
    if (!Array.isArray(ids) || ids.length > 64 || new Set(ids).size !== ids.length ||
      !ids.every(id => typeof id === 'string' && /^[A-Za-z0-9_-]{1,64}$/.test(id)))
      throw new TypeError('ai4math proofSlots must list distinct approved slot IDs.');
    return [role, ids.slice()];
  }));
  const initialSocketPath = config.socketPath || process.env.AI4MATH_SOCKET || undefined;
  const bridge = createBridge({ socketPath: initialSocketPath, timeoutMs: config.timeoutMs });
  const workbench = createWorkbench({
    bridge,
    cliPath: config.cliPath,
    daemonPath: config.daemonPath,
    registryPath: config.registryPath,
    initialSocketPath,
    rolePresets: config.rolePresets ?? {},
    proofSlots,
    defaultChiefPreset: config.defaultChiefPreset ?? 'ai4math-chief',
  });
  ctx.effect(() => async () => {
    await workbench.close();
    bridge.close();
  }, 'ai4math.socket-lifecycle');
  new Ai4mathRemote(ctx, bridge.bind('owner'), workbench);
  ctx.typert.register(contribution);
  installTools(ctx, bridge, roleByPreset, proofSlots);
}
