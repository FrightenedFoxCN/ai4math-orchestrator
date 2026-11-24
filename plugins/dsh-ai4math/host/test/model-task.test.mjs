import assert from 'node:assert/strict';
import test from 'node:test';
import { createModelTaskAssignment, installModelTaskTools } from '../model-task.mjs';

const sig = () => new AbortController().signal;
const packet = (role='prover_worker') => ({version:1, task_id:'T-1', claim_id:'C-1', role,
  engine:'lean4', module_path:'Goal.lean', target_decl:'target', base_commit:'a'.repeat(40),
  max_wall_seconds:2, informal_statement:'The statement is true.', formal_header:'theorem target : True'});

function setup(input=packet()) {
  const agent = {};
  const parent = new AbortController();
  let t = 17;
  const r = createModelTaskAssignment(agent,input,parent.signal,{ now:() => t });
  const execution = {agent,signal:sig()};
  return {agent, parent, r, get:(args={}, exec=execution)=>r.tools[0].execute(args,exec),
    submit:(args,exec=execution)=>r.tools[1].execute(args,exec), advance:ms=>t+=ms};
}

test('an assignment exposes only its immutable packet and accepts one untrusted result', () => {
  const raw = packet();
  const {r,get,submit,agent}=setup(raw);
  raw.claim_id='substitute';
  const provided=get();
  provided.task_id='spoof';
  assert.deepEqual(get(), packet());
  assert.deepEqual(r.tools.map(t=>t.name),['ai4math_task_context','ai4math_task_submit']);
  assert.throws(()=>get({}, {agent:{},signal:sig()}),{code:'denied'});
  assert.throws(()=>submit({proof_body:'True.intro'}, {agent:{},signal:sig()}),{code:'denied'});
  assert.throws(()=>get({task_id:'T-2'}),{code:'invalid'});
  assert.throws(()=>submit({proof_body:'True.intro',base_commit:'b'.repeat(40)}),{code:'invalid'});
  assert.throws(()=>submit({proof_body:'True.intro',proof_verified:true}),{code:'invalid'});
  assert.throws(()=>submit({proof_body:'α'.repeat(4097)}),{code:'invalid'});
  assert.equal(r.takeResult(),null);
  assert.deepEqual(submit({proof_body:'True.intro'}),{accepted:true,verified:false});
  assert.throws(()=>submit({proof_body:'replacement'}),{code:'cancelled'});
  assert.throws(()=>get(),{code:'cancelled'});
  assert.deepEqual(r.takeResult(),{task_id:'T-1',claim_id:'C-1',base_commit:'a'.repeat(40),
    role:'prover_worker',module_path:'Goal.lean',target_decl:'target',
    suggestion:{kind:'proof_body', proof_body:'True.intro'},proof_verified:false});
  assert.equal(r.takeResult(),null);
  assert.throws(()=>createModelTaskAssignment(agent,packet(),sig()),{code:'denied'});
});

test('review surface has strict limited input and cannot provide proof authority', () => {
  const input=packet('independent_reviewer');
  for (const extra of [{writer_history:[]},{proof_body:'private text'},{author:'A'}]) {
    assert.throws(()=>setup({...input,...extra}),{code:'invalid'});
  }
  const {r,get,submit}=setup(input);
  const provided=get();
  assert.equal(provided.formal_header,input.formal_header);
  assert.throws(()=>submit({proof_body:'True.intro'}),{code:'invalid'});
  assert.throws(()=>submit({verdict:'approved_l5',rationale:'bad'}),{code:'invalid'});
  const rationale='The target matches the written statement.';
  assert.deepEqual(submit({verdict:'looks_consistent',rationale}),{accepted:true,verified:false});
  assert.deepEqual(r.takeResult().suggestion,{kind:'review',verdict:'looks_consistent',rationale});
});

test('deadline, parent abort, call abort, and explicit revocation fail closed', () => {
  const stale=setup();
  stale.advance(2000);
  assert.throws(()=>stale.get(),{code:'cancelled'});
  assert.throws(()=>stale.submit({proof_body:'True.intro'}),{code:'cancelled'});
  assert.equal(stale.r.takeResult(),null);
  for(const stop of [x=>x.parent.abort(), x=>x.advance(2000), x=>x.r.revoke()]) {
    const x=setup();
    x.submit({proof_body:'True.intro'});
    stop(x);
    assert.equal(x.r.takeResult(),null);
  }
  const x=setup();
  assert.throws(()=>x.get({}, {agent:x.agent,signal:AbortSignal.abort()}),{code:'cancelled'});
  assert.throws(()=>x.submit({get proof_body() {throw new Error('getter executed');}}),{code:'invalid'});
});

test('the registered surface stays disabled until the exact tool set is audited', () => {
  function setupLayer() {
    const registered = [];
    let rule, guard;
    const agent = {};
    const assignment = installModelTaskTools({
      restrict: value=>{rule=value;}, guard: fn=>{guard=fn;}, register: tool=>registered.push(tool),
    }, agent, packet(), sig());
    assert.deepEqual(rule,{allow:[]});
    assert.equal(guard({name:'ai4math_task_context'}),undefined);
    assert.equal(typeof guard({name:'bash'}),'string');
    return {assignment, registered, get:()=>registered[0].execute({},{agent,signal:sig()})};
  }
  const first=setupLayer();
  const expected=first.registered.map(tool=>tool.name);
  assert.throws(first.get,{code:'denied'});
  first.assignment.assertVisibleTools(expected);
  assert.equal(first.get().task_id,'T-1');
  assert.throws(()=>first.assignment.assertVisibleTools([...expected,'bash']),{code:'denied'});
  assert.throws(first.get,{code:'denied'});
  assert.throws(()=>setupLayer().assignment.assertVisibleTools([expected[0],expected[0]]),{code:'denied'});
});
