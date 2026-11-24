// Internal, operator-provisioned capability for one disposable DSH Agent object.
// No daemon client, filesystem IO, promotion API or preset-wide registration.
import { performance } from 'node:perf_hooks';
import { BridgeError } from './connection.mjs';

const assigned = new WeakSet();
const names = Object.freeze(['ai4math_task_context', 'ai4math_task_submit']);
const ROLES = Object.freeze({ prover_worker: 'proof_body', independent_reviewer: 'review' });
const VERDICTS = Object.freeze(['looks_consistent', 'needs_changes', 'insufficient_context']);
const id = /^[A-Za-z0-9_-]{1,100}$/;
const oid = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/;
const moduleName = /^[A-Z][A-Za-z0-9_]{0,250}\.lean$/;
const declaration = /^[A-Za-z_][A-Za-z0-9_']{0,127}$/;
const invalid = () => { throw new BridgeError('invalid'); };

function record(input, keys) {
  if (input === null || typeof input !== 'object' || ![Object.prototype, null].includes(Object.getPrototypeOf(input))) invalid();
  const actual = Reflect.ownKeys(input);
  if (actual.length !== keys.length || !actual.every((key) => keys.includes(key))) invalid();
  const output = {};
  for (const key of keys) {
    const descriptor = Object.getOwnPropertyDescriptor(input, key);
    if (!descriptor || !Object.hasOwn(descriptor, 'value')) invalid();
    output[key] = descriptor.value;
  }
  return output;
}

function matches(value, pattern) {
  return typeof value === 'string' && !/[\r\n]/u.test(value) && pattern.test(value);
}

function boundedText(value, limit) {
  return typeof value === 'string' && value.length <= limit && value.trim().length > 0
    && Buffer.byteLength(value, 'utf8') <= limit && !/[\u0000-\u0008\u000b-\u001f\u007f-\u009f\ud800-\udfff]/u.test(value);
}

// Deliberately no author identity, session history, proof attempt or raw logs.
// The caller supplies a pre-sliced trusted header, not an entire Lean module.
function taskPacket(packet) {
  const copy = record(packet, ['version','task_id','claim_id','role','engine','module_path','target_decl',
    'base_commit','max_wall_seconds','informal_statement','formal_header']);
  if (copy.version !== 1 || copy.engine !== 'lean4' || typeof copy.role !== 'string' || !Object.hasOwn(ROLES, copy.role)
    || !matches(copy.task_id,id) || !matches(copy.claim_id,id) || !matches(copy.module_path,moduleName)
    || !matches(copy.target_decl,declaration) || !matches(copy.base_commit,oid)
    || !Number.isInteger(copy.max_wall_seconds) || copy.max_wall_seconds < 1 || copy.max_wall_seconds > 60
    || !boundedText(copy.informal_statement, 8192) || !boundedText(copy.formal_header, 16_384)) invalid();
  return Object.freeze(copy);
}

const schema = (properties) => ({ type:'object', additionalProperties:false, properties, required:Object.keys(properties) });
const output = { schema: { type:'object' }, render: (_args, value) => [{ type:'text', text:JSON.stringify(value) }] };
const text = { type:'string', minLength:1, maxLength:8192 };

/** Trusted Host only. Caller must provision a fresh agent with no history or
 * unrelated tool capabilities and dispose it after the run. No model service
 * or durable SQLite lease is created here; signal is the parent run lifetime. */
export function createModelTaskAssignment(agent, rawPacket, signal, clock = performance) {
  if (!agent || typeof agent !== 'object' || !(signal instanceof AbortSignal)) invalid();
  if (assigned.has(agent)) throw new BridgeError('denied');
  const packet = taskPacket(rawPacket);
  if (signal.aborted) throw new BridgeError('cancelled');
  const deadline = clock.now() + packet.max_wall_seconds * 1000;
  assigned.add(agent); // Do not reuse an Agent identity after completion or cancellation.
  let revoked = false;
  let taken = false;
  let completed;
  const live = () => !revoked && !signal.aborted && clock.now() < deadline;
  function assertExecution(execution) {
    if (execution?.agent !== agent || !(execution.signal instanceof AbortSignal)) throw new BridgeError('denied');
    if (!live() || execution.signal.aborted || completed || taken) throw new BridgeError('cancelled');
  }
  const role = packet.role;
  const proving = role === 'prover_worker';
  const submitProperties = proving ? { proof_body: text } : { verdict:{ type:'string', enum:VERDICTS }, rationale:text };
  const tools = [
    {
      name:names[0], description:'Read only the current assigned Lean task, not project history.',
      parameters:schema({}), output,
      execute(args, execution) {
        assertExecution(execution);
        record(args, []);
        return { ...packet };
      },
    },
    {
      name:names[1], description:proving
        ? 'Submit one unverified Lean proof-body suggestion, never a proof receipt or promotion.'
        : 'Submit one advisory independent review verdict, never a proof receipt or promotion.',
      parameters:schema(submitProperties), output,
      execute(args, execution) {
        assertExecution(execution);
        const suggestion = record(args, Object.keys(submitProperties));
        if (proving ? !boundedText(suggestion.proof_body,8192)
          : !VERDICTS.includes(suggestion.verdict) || !boundedText(suggestion.rationale,8192)) invalid();
        // No await between validation and completion: a later call cannot replace it.
        completed = Object.freeze({ kind:ROLES[role], ...suggestion });
        return { accepted:true, verified:false };
      },
    },
  ];
  return Object.freeze({
    tools:Object.freeze(tools),
    revoke() { revoked = true; completed = undefined; },
    takeResult() {
      if (!live() || taken || completed === undefined) return null;
      taken = true;
      const result = { task_id:packet.task_id, claim_id:packet.claim_id, base_commit:packet.base_commit,
        role, module_path:packet.module_path, target_decl:packet.target_decl,
        suggestion:{...completed}, proof_verified:false };
      completed = undefined;
      return result;
    },
  });
}

/** Install in a freshly created, disposable Agent context, before the first
 * turn. The host selects the native executor separately and must audit the
 * actual model-facing request; tool-scope filtering is not an OS sandbox. */
export function installModelTaskTools(agentTools, agent, packet, signal) {
  for (const key of ['restrict', 'guard', 'register']) {
    if (typeof agentTools?.[key] !== 'function') throw new BridgeError('unconfigured');
  }
  const assignment = createModelTaskAssignment(agent, packet, signal);
  let audited = false;
  try {
    agentTools.restrict({ allow:[] });
    agentTools.guard((execution) => names.includes(execution.name) ? undefined : 'Not an assigned ai4math task tool.');
    for (const tool of assignment.tools) agentTools.register({
      ...tool,
      execute(args, execution) {
        if (!audited) throw new BridgeError('denied');
        return tool.execute(args, execution);
      },
    });
  } catch (error) {
    assignment.revoke();
    throw error;
  }
  return Object.freeze({
    revoke:assignment.revoke,
    takeResult:assignment.takeResult,
    // Check after mount and before issuing a model turn. Own-layer tools
    // survive restrict(); unknown own-layer tools are never safe to advertise.
    assertVisibleTools(visible) {
      if (!Array.isArray(visible) || visible.length !== names.length || !names.every((name) => visible.includes(name))) {
        audited = false;
        assignment.revoke();
        throw new BridgeError('denied');
      }
      audited = true;
    },
  });
}
