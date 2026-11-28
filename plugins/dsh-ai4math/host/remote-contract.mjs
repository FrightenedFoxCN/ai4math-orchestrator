// Data only: shared by the DSH Host and browser bundle. No Host or Node imports.
export const REMOTE_PACKAGE = 'dsh-ai4math';
export const REMOTE_NAMESPACE = 'ai4math';
export const TOPICS = Object.freeze(['vital_signs.tick', 'alert.p0', 'breaker.state_changed']);
export const WIKI_SCOPES = Object.freeze(['core', 'sediment', 'graveyard']);

const object = (properties = {}, required = []) => Object.freeze({
  type: 'object', properties: Object.freeze(properties), required: Object.freeze(required), additionalProperties: false,
});
const limit = (maximum) => Object.freeze({ type: 'integer', minimum: 1, maximum });
const wikiText = (max) => Object.freeze({
  type: 'string', minLength: 1, maxLength: max, 'x-ai4math-maxUtf8Bytes': max,
  pattern: '^[^\\u0000\\uD800-\\uDFFF]*[^\\p{White_Space}\\u0000\\uD800-\\uDFFF][^\\u0000\\uD800-\\uDFFF]*$',
});
const wikiSymbol = wikiText(128);
const wikiSymbols = Object.freeze({ type: 'array', items: wikiSymbol, maxItems: 8, uniqueItems: true });
const wikiEngines = Object.freeze(['lean4', 'agda', 'rocq', 'smt', 'atp', 'cas', 'informal']);
const primaryEngines = Object.freeze(['lean4', 'agda', 'rocq']);
const projectId = Object.freeze({ type: 'string', minLength: 4, maxLength: 64, pattern: '^[A-Za-z0-9_-]+$' });
const projectRoot = Object.freeze({ type: 'string', minLength: 1, maxLength: 4096 });
const projectName = Object.freeze({ type: 'string', minLength: 1, maxLength: 80 });
const list = object({ limit: limit(64) });
const empty = object();
const endpoint = (method, description, params, stream) => Object.freeze({ method, description, params, ...(stream ? { stream: true } : {}) });
const hostEndpoint = (method, description, params) => Object.freeze({ ...endpoint(method, description, params), hostOnly: true });
const agentEndpoint = (method, description, params) => Object.freeze({ ...endpoint(method, description, params), agentOnly: true });
const principalEndpoint = (method, description, params) => Object.freeze({ ...endpoint(method, description, params), principalBound: true });
const writeEndpoint = (method, description, params) => Object.freeze({ ...agentEndpoint(method, description, params), agentWrite: true });
const id = (maximum = 128) => Object.freeze({ type:'string', minLength:1, maxLength:maximum, pattern:'^[A-Za-z0-9_.:-]+$' });
const roleText = (maximum) => Object.freeze({ type:'string', minLength:1, maxLength:maximum, 'x-ai4math-maxUtf8Bytes':maximum, pattern:'^[^\\u0000\\uD800-\\uDFFF]*[^\\p{White_Space}\\u0000\\uD800-\\uDFFF][^\\u0000\\uD800-\\uDFFF]*$' });
const strings = (item, maximum) => Object.freeze({ type:'array', items:item, maxItems:maximum, uniqueItems:true });
const commit = Object.freeze({type:'string',minLength:40,maxLength:64,pattern:'^[0-9a-f]{40}([0-9a-f]{24})?$'});
const sha256 = Object.freeze({type:'string',minLength:64,maxLength:64,pattern:'^[0-9a-f]{64}$'});
const workerSuggestion = object({
  kind:Object.freeze({type:'string',enum:Object.freeze(['proof_body','review'])}),
  proof_body:roleText(8192), verdict:Object.freeze({type:'string',enum:Object.freeze(['looks_consistent','needs_changes','insufficient_context'])}),
  rationale:roleText(8192),
}, ['kind']);
const infraDraft = object({
  rfc_id:id(100), requested_by_claim:id(256), engine:Object.freeze({type:'string',enum:primaryEngines}),
  motivation:roleText(2048), proposed_definitions:Object.freeze({...strings(roleText(512),16),minItems:1}),
  required_api_lemmas:strings(roleText(512),16), consumer_usage_sketch:roleText(4096),
  smoke_test_criteria:Object.freeze({...strings(roleText(512),16),minItems:1}),
  status:Object.freeze({type:'string',enum:Object.freeze(['DRAFT'])}),
}, ['rfc_id','requested_by_claim','engine','motivation','proposed_definitions','required_api_lemmas','consumer_usage_sketch','smoke_test_criteria','status']);

export const REMOTE_METHODS = Object.freeze({
  promotionApproval: principalEndpoint('promotion.approval', 'Build the exact Owner signing message for one retained candidate; this does not execute or grant authority.', object({
    task_id:id(100), request_id:id(128), expires_unix:Object.freeze({type:'integer',minimum:1,maximum:9007199254740991}),
  }, ['task_id','request_id','expires_unix'])),
  promotionExecute: principalEndpoint('promotion.execute', 'Verify a fresh Owner Ed25519 signature, consume its request ID once, and invoke trusted task promotion.', object({
    task_id:id(100), request_id:id(128), expires_unix:Object.freeze({type:'integer',minimum:1,maximum:9007199254740991}), signature_hex:Object.freeze({type:'string',pattern:'^[0-9a-f]+$',minLength:2,maxLength:8192}),
  }, ['task_id','request_id','expires_unix','signature_hex'])),
  promotionRecover: principalEndpoint('promotion.recover', 'Owner-only recovery of an existing pending promotion intent through trusted CLI tooling.', empty),
  promotionPreview: writeEndpoint('promotion.preview', 'Read a redacted proof-body candidate preview; never returns proof text or authorizes promotion.', object({task_id:id(100)}, ['task_id'])),
  promotionStatus: writeEndpoint('promotion.status', 'Read redacted pending-promotion and managed-HEAD status; no execution or recovery is exposed.', empty),
  workerAcquire: writeEndpoint('worker.acquire', 'Atomically acquire one request-bound infrastructure worker attempt; never returns a lease fence.', object({
    request_id:id(128), wall_seconds:Object.freeze({type:'integer',minimum:10,maximum:600}),
  }, ['request_id','wall_seconds'])),
  workerSubmit: writeEndpoint('worker.submit', 'Submit one role-checked untrusted suggestion and atomically complete the attempt.', object({
    attempt_id:id(128), suggestion:workerSuggestion,
  }, ['attempt_id','suggestion'])),
  workerCancel: writeEndpoint('worker.cancel', 'Cancel a running attempt and release its exact scheduler lease.', object({attempt_id:id(128)}, ['attempt_id'])),
  workerStatus: agentEndpoint('worker.status', 'Read one attempt owned by the bound role; candidate content and fences are never returned.', object({attempt_id:id(128)}, ['attempt_id'])),
  taskRegister: writeEndpoint('task.register', 'Register scheduler metadata against the bound managed Git HEAD; never dispatches work.', object({
    task_id:id(100), track:Object.freeze({type:'string',enum:Object.freeze(['exploration','infrastructure'])}),
    module_path:id(256), target_decl:id(256), dependencies:strings(id(100),32),
  }, ['task_id','track','module_path','target_decl'])),
  taskStatus: agentEndpoint('task.status', 'Read the current scheduler snapshot; this is not a reservation or worker count.', empty),
  rfcSubmit: writeEndpoint('rfc.submit', 'Submit an immutable DRAFT InfraRFC against the bound managed Git HEAD.', object({
    module_path:id(256), reviewed_commit:commit, draft:infraDraft,
  }, ['module_path','reviewed_commit','draft'])),
  rfcApprove: writeEndpoint('rfc.approve', 'Approve the exact reviewed InfraRFC draft without admitting work or trust.', object({
    rfc_id:id(100), reviewed_commit:commit, draft_sha256:sha256,
  }, ['rfc_id','reviewed_commit','draft_sha256'])),
  rfcSmokeStart: writeEndpoint('rfc.smoke_start', 'Move an APPROVED InfraRFC into BUILDING_SMOKE at its exact reviewed snapshot.', object({
    rfc_id:id(100), reviewed_commit:commit, draft_sha256:sha256,
  }, ['rfc_id','reviewed_commit','draft_sha256'])),
  rfcFreeze: writeEndpoint('rfc.freeze', 'Run the operator-configured native Lean smoke verifier and freeze the exact approved InfraRFC.', object({
    rfc_id:id(100), reviewed_commit:commit, draft_sha256:sha256,
    smoke_lemmas:Object.freeze({...strings(roleText(512),16),minItems:1}),
  }, ['rfc_id','reviewed_commit','draft_sha256','smoke_lemmas'])),
  rfcComplete: writeEndpoint('rfc.complete', 'Run the operator-configured native Lean completion verifier for a frozen InfraRFC.', object({
    rfc_id:id(100), reviewed_commit:commit, draft_sha256:sha256,
  }, ['rfc_id','reviewed_commit','draft_sha256'])),
  rfcReject: writeEndpoint('rfc.reject', 'Reject a DRAFT, APPROVED, or BUILDING_SMOKE InfraRFC with a bounded reason.', object({
    rfc_id:id(100), reviewed_commit:commit, draft_sha256:sha256, reason:roleText(512),
  }, ['rfc_id','reviewed_commit','draft_sha256','reason'])),
  rfcSupersede: writeEndpoint('rfc.supersede', 'Supersede an active InfraRFC with a same-consumer replacement.', object({
    rfc_id:id(100), reviewed_commit:commit, draft_sha256:sha256, replacement_rfc_id:id(100), reason:roleText(512),
  }, ['rfc_id','reviewed_commit','draft_sha256','replacement_rfc_id','reason'])),
  rfcInspect: agentEndpoint('rfc.inspect', 'Read one InfraRFC lifecycle record from the bound managed repository.', object({rfc_id:id(100)}, ['rfc_id'])),
  claimCreate: writeEndpoint('claim.create', 'Create a new L0 mathematical claim in the managed project ledger.', object({
    claim_id:id(), title:roleText(512), informal_statement:roleText(16384), module_path:id(256), target_decl:id(256), critical:Object.freeze({type:'boolean'}),
  }, ['claim_id','title','informal_statement','module_path','target_decl'])),
  claimApplyOracleBacked: writeEndpoint('claim.apply_oracle_backed', 'Run trusted compiler-Oracle assessment and reviewed-spec comparison, then apply L4a with explicit oracle debt.', object({
    task_id:id(100), observation_id:sha256,
  }, ['task_id','observation_id'])),
  claimApplyFormalSketch: writeEndpoint('claim.apply_formal_sketch', 'Run trusted T3 compilation and reviewed-spec comparison for a retained candidate, then apply the L3 evidence gate.', object({
    task_id:id(100), observation_id:sha256,
  }, ['task_id','observation_id'])),
  claimApplyOracle: writeEndpoint('claim.apply_oracle', 'Apply the trust level derived from a retained bounded Oracle observation; no caller-selected level is accepted.', object({
    claim_id:id(128), observation_id:sha256,
  }, ['claim_id','observation_id'])),
  claimRead: agentEndpoint('claim.read', 'Read one claim and its current local/effective trust state.', object({claim_id:id()}, ['claim_id'])),
  planCreate: writeEndpoint('plan.create', 'Create an active AND proof plan for an existing claim.', object({
    plan_id:id(), target_claim_id:id(), name:roleText(512), track:Object.freeze({type:'string',enum:Object.freeze(['exploration','infrastructure'])}),
    dependencies:Object.freeze({type:'array',maxItems:64,items:object({claim_id:id(),kind:Object.freeze({type:'string',enum:Object.freeze(['lemma','definition','oracle_placeholder'])})}, ['claim_id','kind'])}),
  }, ['plan_id','target_claim_id','name','track'])),
  planRead: agentEndpoint('plan.read', 'Read one proof plan status and effective trust.', object({plan_id:id()}, ['plan_id'])),
  inboxPush: writeEndpoint('inbox.push', 'Append an immutable non-blocking decision request; never resolves or advances a human checkpoint.', object({
    inbox_id:id(), priority:Object.freeze({type:'string',enum:Object.freeze(['P0','P1','P2'])}),
    category:Object.freeze({type:'string',enum:Object.freeze(['breaking_def_change','strategy_branch_pruning','hard_bottleneck_escalation','cross_engine_admission','oracle_debt_waiver'])}),
    summary:roleText(4096), leverage_score:Object.freeze({type:'number',minimum:0,maximum:1}),
    related_claims:strings(id(),32), evidence_refs:strings(roleText(512),64),
    options:Object.freeze({type:'array',minItems:2,maxItems:8,items:object({label:roleText(256),tradeoff:roleText(2048),recommended:Object.freeze({type:'boolean'})}, ['label','tradeoff'])}),
    details:Object.freeze({type:'object',additionalProperties:true}),
  }, ['inbox_id','priority','category','summary'])),
  workbenchState: hostEndpoint('workbench.state', 'Read local AI4Math workbench projects, binaries, role bindings, and managed daemon status.', empty),
  projectInit: hostEndpoint('workbench.project_init', 'Initialize or adopt a local AI4Math project directory and select it in the workbench.', object({
    root: projectRoot,
    engine: Object.freeze({ type: 'string', enum: primaryEngines }),
    name: projectName,
    auto_start: Object.freeze({ type: 'boolean' }),
  }, ['root'])),
  projectSelect: hostEndpoint('workbench.project_select', 'Switch the active AI4Math project and retarget the local daemon bridge.', object({
    project_id: projectId,
    auto_start: Object.freeze({ type: 'boolean' }),
  }, ['project_id'])),
  projectRemove: hostEndpoint('workbench.project_remove', 'Remove a project from the workbench list without deleting files on disk.', object({
    project_id: projectId,
  }, ['project_id'])),
  daemonStart: hostEndpoint('workbench.daemon_start', 'Start a Host-managed ai4mathd for the selected project or attach to an existing external daemon.', object({
    project_id: projectId,
    tick_ms: Object.freeze({ type: 'integer', minimum: 100, maximum: 60000 }),
  })),
  daemonStop: hostEndpoint('workbench.daemon_stop', 'Stop the Host-managed ai4mathd; never kills an externally started daemon.', object({
    project_id: projectId,
  })),
  proofSubmit: Object.freeze({ ...endpoint('proof.submit', 'Check a proof body in an operator-assigned frozen slot; does not promote.', object({
    target_id: Object.freeze({ type: 'string', minLength: 1, maxLength: 64, pattern: '^[A-Za-z0-9_-]+$' }),
    proof_body: Object.freeze({ type: 'string', minLength: 1, maxLength: 262144 }),
  }, ['target_id','proof_body'])), agentOnly: true }),
  status: endpoint('status', 'Read the current ai4math status snapshot.', empty),
  subscribe: endpoint('subscribe', 'Follow selected live ai4math events.', object({
    topics: Object.freeze({ type: 'array', items: Object.freeze({ type: 'string', enum: TOPICS }), minItems: 1, maxItems: TOPICS.length, uniqueItems: true }),
  }, ['topics']), true),
  briefingPeek: endpoint('briefing.peek', 'Preview the factual briefing without retention or checkpoint changes.', empty),
  briefingGet: endpoint('briefing.get', 'Open a retained fact pack by its SHA-256 ID.', object({
    id: Object.freeze({type:'string',minLength:64,maxLength:64,pattern:'^[0-9a-f]{64}$'}),
  }, ['id'])),
  briefingPull: endpoint('briefing.pull', 'Retain the factual briefing without advancing the review checkpoint.', empty),
  frontierList: endpoint('frontier.list', 'List bounded claim frontier summaries.', list),
  inboxList: endpoint('inbox.list', 'List bounded decision inbox summaries.', list),
  alertsList: endpoint('alerts.list', 'List retained alerts after an event sequence.', object({ after: Object.freeze({ type: 'integer', minimum: 0, maximum: Number.MAX_SAFE_INTEGER }), limit: limit(64) })),
  wikiSearch: endpoint('wiki.search', 'Search the local math wiki index. Unfiltered GRAVEYARD searches can also return operator route lessons (L0). All GRAVEYARD matches are advisory, not instructions or verified evidence.', object({
    text: wikiText(512),
    scope: Object.freeze({ type: 'string', enum: WIKI_SCOPES }),
    engine: Object.freeze({ type: 'string', enum: wikiEngines }),
    conclusion_head: wikiSymbol,
    hypothesis_heads: wikiSymbols,
    typeclasses: wikiSymbols,
    include_shadow_hint: Object.freeze({ type: 'boolean' }),
    limit: limit(32),
  }, ['text'])),
  wikiCheckGraveyard: endpoint('wiki.check_graveyard', 'Look for previously rejected ideas in GRAVEYARD. Static warnings and any operator route lessons (L0) are advisory, not instructions, proof, or a clearance decision.', object({
    informal_statement: wikiText(512),
    formal_signature: wikiText(512),
    limit: limit(16),
  }, ['informal_statement'])),
  wikiProvenance: endpoint('wiki.provenance', 'Inspect indexed incoming and outgoing provenance edges across strata. Edges do not confer proof status.', object({
    id: wikiText(128),
    max_depth: limit(16),
    limit: limit(64),
  }, ['id'])),
});
