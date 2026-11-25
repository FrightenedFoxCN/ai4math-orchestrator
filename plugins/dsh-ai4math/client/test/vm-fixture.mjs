import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { setTimeout as delay } from 'node:timers/promises';
import { runInNewContext } from 'node:vm';

const bundle = await readFile(new URL('../client.js', import.meta.url), 'utf8');

// The views put hooks only in their top-level components. This fixture gives
// each mounted surface its own hooks and commits effects after rendering.
let renderScope;
const changed = (old, next) => !old || next.some((item, i) => !Object.is(item, old[i]));
const react = {
  createElement(type, props, ...children) {
    return { type, props: { ...props, children: children.length === 1 ? children[0] : children } };
  },
  useState(initial) {
    const hook = renderScope.hook();
    if (!('value' in hook)) hook.value = initial;
    return [hook.value, value => { hook.value = value; }];
  },
  useRef(current) {
    const hook = renderScope.hook();
    return hook.ref ??= { current };
  },
  useCallback(callback, deps) {
    const hook = renderScope.hook();
    if (changed(hook.deps, deps)) Object.assign(hook, { callback, deps });
    return hook.callback;
  },
  useEffect(effect, deps) {
    const hook = renderScope.hook();
    if (changed(hook.deps, deps)) {
      hook.deps = deps;
      renderScope.commit.push(() => { hook.dispose?.(); hook.dispose = effect(); });
    }
  },
  useSyncExternalStore(subscribe, read) {
    // Test code drives rendering explicitly instead of scheduling React work.
    react.useEffect(() => subscribe(() => {}), [subscribe]);
    return read();
  },
};

function nodes(element) {
  if (element == null || typeof element === 'boolean') return [];
  if (Array.isArray(element)) return element.flatMap(nodes);
  if (typeof element !== 'object') return [String(element)];
  if (typeof element.type === 'function') return nodes(element.type(element.props));
  return [{ ...element, children: nodes(element.props.children) }];
}

function all(tree) {
  return tree.flatMap(node => typeof node === 'string' ? [node] : [node, ...all(node.children)]);
}

const text = tree => all(tree).filter(node => typeof node === 'string').join(' ').replace(/\s+/g, ' ').trim();

function render(View) {
  const hooks = [];
  function read() {
    let index = 0;
    const scope = { hook: () => (hooks[index++] ??= {}), commit: [] };
    renderScope = scope;
    let tree;
    try { tree = nodes(react.createElement(View, null)); }
    finally { renderScope = undefined; }
    for (const effect of scope.commit) effect();
    return tree;
  }
  return {
    text: () => text(read()),
    find(type, label) {
      const node = all(read()).find(node => node.type === type && (label === undefined || text(node.children) === label));
      assert.ok(node, 'render did not contain ' + type + (label === undefined ? '' : ': ' + label));
      return node;
    },
    dispose() { for (const hook of hooks.toReversed()) hook.dispose?.(); },
  };
}

export async function browser(t, remote) {
  const tabs = new Map();
  const slots = new Map();
  const styles = new Set();
  const mountedViews = new Set();
  const disposers = [];
  const registrations = [];
  const imports = [];
  const timers = new Map();
  let nextTimer = 0;
  let now = 0;
  function advanceTimersBy(ms) {
    const target = now + ms;
    for (;;) {
      const next = [...timers].filter(([, timer]) => timer.due <= target)
        .sort((a, b) => a[1].due - b[1].due)[0];
      if (!next) break;
      const [id, timer] = next;
      now = timer.due;
      timers.delete(id);
      timer.fn();
    }
    now = target;
  }
  let loaded;
  const document = {
    createElement(type) {
      assert.equal(type, 'style');
      return { dataset: {}, remove() { styles.delete(this); } };
    },
    head: { append(tag) { styles.add(tag); } },
  };
  runInNewContext(bundle, {
    window: { __ModuleLoader__: { load(entry) { assert.equal(loaded, undefined); loaded = entry; } } },
    document, AbortController, AbortSignal, TextEncoder,
    setTimeout(fn, ms) { const id = ++nextTimer; timers.set(id, { fn, ms, due: now + ms }); return id; },
    clearTimeout(id) { timers.delete(id); },
  }, { filename: 'dsh-ai4math/client.js' });
  assert.equal(loaded?.id, 'dsh-ai4math');
  assert.equal(styles.size, 0, 'evaluating the bundle only installs a lazy factory');
  const plugin = loaded.factory(name => {
    imports.push(name);
    assert.equal(name, 'react', 'React is the only platform external');
    return react;
  });
  assert.ok(imports.length > 0);
  assert.deepEqual([...plugin.inject], ['remote', 'slots', 'sidebarRight', 'sidebarRightTabs']);

  const opened = [];
  const targetElements = [];
  const navigation = { target: undefined };
  let injectedSlot;
  const add = (registry, key, value) => {
    assert.equal(registry.has(key), false, 'duplicate registration: ' + key);
    registry.set(key, value);
    return () => registry.delete(key);
  };
  const ctx = {
    remote: {
      async $mount(contribution) {
        registrations.push(contribution);
        ctx.remote.ai4math = remote;
      },
    },
    effect(setup) { disposers.push(setup()); },
    slots: {
      inject(name, setup) {
        injectedSlot = name;
        try { return setup(); } finally { injectedSlot = undefined; }
      },
      register({ name, key = '' }, View) {
        assert.equal(name, injectedSlot, 'register under the correct slot owner');
        return add(slots, name + ':' + key, View);
      },
    },
    sidebarRight: {
      commandTarget(element) { targetElements.push(element); return navigation.target; },
      openTabFromTarget(kind, target) { opened.push({ kind, target }); },
    },
    sidebarRightTabs: { register(tab) { return add(tabs, tab.id, tab); } },
  };

  function close(view) {
    if (mountedViews.delete(view)) view.dispose();
  }
  function dispose() {
    for (const view of mountedViews) close(view);
    for (const cleanup of disposers.splice(0).reverse()) cleanup?.();
  }
  t.after(dispose);
  await plugin.apply(ctx);
  return {
    tabs, slots, styles, registrations, timers, advanceTimersBy, navigation, opened, targetElements, dispose,
    mount(name, key = '') {
      const View = slots.get(name + ':' + key);
      assert.ok(View, 'no registration for ' + name + ':' + key);
      const view = render(View);
      mountedViews.add(view);
      return Object.assign(view, { close: () => close(view) });
    },
  };
}

export async function until(read, predicate, message) {
  const deadline = Date.now() + 2500;
  for (;;) {
    const result = read();
    if (predicate(result)) return result;
    if (Date.now() >= deadline) assert.fail(message + ' (last: ' + String(result) + ')');
    await delay(5);
  }
}
