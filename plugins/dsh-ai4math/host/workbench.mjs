import { execFile, spawn } from 'node:child_process';
import { createHash, randomBytes } from 'node:crypto';
import { accessSync, constants, existsSync, mkdirSync, readFileSync, realpathSync, renameSync, statSync, writeFileSync } from 'node:fs';
import { lstat, unlink } from 'node:fs/promises';
import { createConnection } from 'node:net';
import { homedir } from 'node:os';
import { basename, dirname, isAbsolute, join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';
import { BridgeError } from './connection.mjs';

const execFileAsync = promisify(execFile);
const ENGINES = new Set(['lean4', 'agda', 'rocq']);
const MAX_PROJECTS = 32;
const MAX_LOG_LINES = 60;
const MAX_LOG_CHARS = 240;
const MAX_SOCKET_BYTES = process.platform === 'linux' ? 107 : 103;
const PLUGIN_ROOT = fileURLToPath(new URL('../', import.meta.url));

function isExecutable(filePath) {
  if (typeof filePath !== 'string' || !isAbsolute(filePath) || filePath.includes('\0')) return false;
  try {
    const stat = statSync(filePath);
    if (!stat.isFile()) return false;
    accessSync(filePath, constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function resolveBinary(configured, envName, binaryName) {
  const candidates = [
    configured,
    process.env[envName],
    join(PLUGIN_ROOT, '..', '..', 'target', 'release', binaryName),
    join(PLUGIN_ROOT, '..', '..', 'target', 'debug', binaryName),
  ].filter((value) => typeof value === 'string' && value.length > 0);
  for (const candidate of candidates) {
    if (isExecutable(candidate)) return { path: candidate, ready: true };
  }
  return { path: candidates[0] ?? null, ready: false };
}

function defaultRegistryPath() {
  const home = process.env.DSH_HOME && isAbsolute(process.env.DSH_HOME)
    ? process.env.DSH_HOME
    : join(homedir(), '.dsh');
  return join(home, 'ai4math', 'workbench.json');
}

function projectIdFor(root) {
  return 'proj-' + createHash('sha256').update(root, 'utf8').digest('hex').slice(0, 12);
}

function sanitizeLine(line) {
  return String(line).replace(/[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/g, ' ').trim().slice(0, MAX_LOG_CHARS);
}

function readEngineMarker(root) {
  try {
    const raw = readFileSync(join(root, '.runtime', 'primary_engine'), 'utf8').trim();
    return ENGINES.has(raw) ? raw : null;
  } catch {
    return null;
  }
}

function isInitializedProject(root) {
  return existsSync(join(root, '.runtime', 'state.sqlite'));
}

function normalizeManagedGit(raw) {
  if (!raw || typeof raw !== 'object' || raw.bound !== true || typeof raw.repo !== 'string' || !isAbsolute(raw.repo)) return null;
  return {
    bound: true,
    repo: raw.repo,
    anchor_module: typeof raw.anchor_module === 'string' ? raw.anchor_module : null,
    base_commit: typeof raw.base_commit === 'string' ? raw.base_commit : null,
  };
}

function readManagedGit(root) {
  try {
    const raw = JSON.parse(readFileSync(join(root, '.runtime', 'managed-git.json'), 'utf8'));
    return normalizeManagedGit({ ...raw, bound: true });
  } catch {
    return null;
  }
}

function socketPathFor(root) {
  return join(root, '.runtime', 'ai4mathd.sock');
}

export function checkSocketPathLength(root) {
  const socketPath = socketPathFor(root);
  const bytes = Buffer.byteLength(socketPath, 'utf8');
  return { socketPath, bytes, limit: MAX_SOCKET_BYTES, ok: bytes <= MAX_SOCKET_BYTES };
}

async function probeSocket(socketPath, timeoutMs = 400) {
  if (typeof socketPath !== 'string' || !isAbsolute(socketPath) || Buffer.byteLength(socketPath, 'utf8') > MAX_SOCKET_BYTES) {
    return 'invalid';
  }
  let stat;
  try {
    stat = await lstat(socketPath);
  } catch {
    return 'stopped';
  }
  if (!stat.isSocket()) return 'conflict';
  return new Promise((resolve) => {
    let settled = false;
    const done = (value) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      socket.destroy();
      resolve(value);
    };
    const socket = createConnection({ path: socketPath });
    const timer = setTimeout(() => done('stale'), timeoutMs);
    socket.once('connect', () => done('live'));
    socket.once('error', () => done('stale'));
  });
}

function inferProjectRootFromSocket(socketPath) {
  if (typeof socketPath !== 'string' || !isAbsolute(socketPath)) return null;
  if (basename(socketPath) !== 'ai4mathd.sock') return null;
  const runtimeDir = dirname(socketPath);
  if (basename(runtimeDir) !== '.runtime') return null;
  const root = dirname(runtimeDir);
  try {
    return statSync(root).isDirectory() ? realpathSync(root) : null;
  } catch {
    return null;
  }
}

function normalizeProjectRecord(raw) {
  if (!raw || typeof raw !== 'object' || Array.isArray(raw)) return null;
  if (typeof raw.root !== 'string' || !isAbsolute(raw.root) || raw.root.includes('\0')) return null;
  const engine = ENGINES.has(raw.engine) ? raw.engine : 'lean4';
  const name = typeof raw.name === 'string' && raw.name.trim().length > 0
    ? raw.name.trim().slice(0, 80) : basename(raw.root) || 'math-project';
  const id = typeof raw.id === 'string' && /^[A-Za-z0-9_-]{4,64}$/.test(raw.id)
    ? raw.id : projectIdFor(raw.root);
  return {
    id,
    name,
    root: raw.root,
    engine,
    socket_path: socketPathFor(raw.root),
    created_at: typeof raw.created_at === 'string' ? raw.created_at : new Date().toISOString(),
    updated_at: typeof raw.updated_at === 'string' ? raw.updated_at : new Date().toISOString(),
    managed_git: normalizeManagedGit(raw.managed_git),
  };
}

function loadRegistry(filePath) {
  try {
    const parsed = JSON.parse(readFileSync(filePath, 'utf8'));
    if (!parsed || typeof parsed !== 'object') return { version: 1, active_project_id: null, projects: [] };
    const seen = new Set();
    const projects = [];
    for (const item of Array.isArray(parsed.projects) ? parsed.projects : []) {
      const normalized = normalizeProjectRecord(item);
      if (!normalized || seen.has(normalized.root)) continue;
      seen.add(normalized.root);
      projects.push(normalized);
      if (projects.length >= MAX_PROJECTS) break;
    }
    const active = typeof parsed.active_project_id === 'string' && projects.some((p) => p.id === parsed.active_project_id)
      ? parsed.active_project_id : null;
    return { version: 1, active_project_id: active, projects };
  } catch {
    return { version: 1, active_project_id: null, projects: [] };
  }
}

function saveRegistry(filePath, registry) {
  const dir = dirname(filePath);
  mkdirSync(dir, { recursive: true, mode: 0o700 });
  const temp = join(dir, `.workbench-${process.pid}-${randomBytes(4).toString('hex')}.tmp`);
  const payload = JSON.stringify({
    version: 1,
    active_project_id: registry.active_project_id,
    projects: registry.projects,
  }, null, 2) + '\n';
  writeFileSync(temp, payload, { mode: 0o600, encoding: 'utf8' });
  renameSync(temp, filePath);
}

/**
 * Owner-only Host workbench controller: manages the local math project registry,
 * project initialization via the trusted `ai4math` CLI, supervised `ai4mathd`
 * lifecycle (never killing externally started daemons), and live bridge retargeting.
 */
export function createWorkbench({
  bridge,
  cliPath,
  daemonPath,
  registryPath,
  initialSocketPath,
  rolePresets = {},
  proofSlots = {},
  defaultChiefPreset = 'ai4math-chief',
} = {}) {
  const storePath = registryPath ?? defaultRegistryPath();
  const registry = loadRegistry(storePath);
  const cli = resolveBinary(cliPath, 'AI4MATH_CLI', 'ai4math');
  const daemonBin = resolveBinary(daemonPath, 'AI4MATH_DAEMON', 'ai4mathd');

  // If no explicit socketPath was supplied in plugin Config, restore the last
  // active project from the persisted Workbench registry so restarts are seamless.
  if (!initialSocketPath && registry.active_project_id) {
    const active = registry.projects.find((p) => p.id === registry.active_project_id);
    if (active && existsSync(active.root) && checkSocketPathLength(active.root).ok) {
      try { bridge.retarget(active.socket_path); } catch { /* ignore invalid saved path */ }
    }
  }

  let managed = null; // { projectId, root, socketPath, child, pid, startedAt, logs, exiting, closed }
  let lastExit = null;
  let opLock = Promise.resolve();

  function serialized(fn) {
    const next = opLock.catch(() => {}).then(fn);
    opLock = next.catch(() => {});
    return next;
  }

  function appendLog(bucket, chunk) {
    const text = chunk.toString('utf8');
    for (const raw of text.split(/\r?\n/)) {
      const clean = sanitizeLine(raw);
      if (!clean) continue;
      bucket.push(clean);
      if (bucket.length > MAX_LOG_LINES) bucket.shift();
    }
  }

  async function stopManagedDaemon() {
    const current = managed;
    if (!current) return { stopped: false, reason: 'not_managed' };
    current.exiting = true;
    const { child, closed, socketPath } = current;
    if (bridge?.socketPath?.() === socketPath) bridge.disconnect?.();
    if (child.exitCode === null && child.signalCode === null) {
      try { child.kill('SIGTERM'); } catch { /* already gone */ }
      const deadline = Promise.race([
        closed.then(() => true),
        delay(2500).then(() => false),
      ]);
      const exited = await deadline;
      if (!exited && child.exitCode === null && child.signalCode === null) {
        try { child.kill('SIGKILL'); } catch { /* already gone */ }
        await Promise.race([closed, delay(1500)]);
      }
    }
    if (managed === current) managed = null;
    // Remove stale socket only if we owned this daemon and nothing is listening on it.
    if (await probeSocket(socketPath) === 'stale') {
      try {
        const stat = await lstat(socketPath);
        if (stat.isSocket()) await unlink(socketPath);
      } catch { /* ignore race */ }
    }
    return { stopped: true, project_id: current.projectId };
  }

  function findProject(projectId) {
    if (projectId) return registry.projects.find((p) => p.id === projectId) ?? null;
    if (registry.active_project_id) {
      const byId = registry.projects.find((p) => p.id === registry.active_project_id);
      if (byId) return byId;
    }
    const currentSocket = bridge.socketPath();
    if (currentSocket) {
      const bySocket = registry.projects.find((p) => p.socket_path === currentSocket);
      if (bySocket) return bySocket;
    }
    return null;
  }

  async function startManagedForProject(project, tickMs = 2000) {
    if (!project) throw new BridgeError('invalid');
    const lengthCheck = checkSocketPathLength(project.root);
    if (!lengthCheck.ok) {
      return {
        ok: false,
        code: 'socket_path_too_long',
        message: `Socket path is ${lengthCheck.bytes} bytes (limit ${lengthCheck.limit}). Choose a shorter project path.`,
      };
    }
    if (!existsSync(project.root)) {
      return { ok: false, code: 'missing_project', message: 'Project directory does not exist on disk.' };
    }
    const currentProbe = await probeSocket(project.socket_path);
    if (currentProbe === 'live') {
      const isOurs = managed && managed.projectId === project.id && managed.child.exitCode === null;
      bridge.retarget(project.socket_path);
      return { ok: true, mode: isOurs ? 'managed' : 'external', pid: isOurs ? managed.pid : null };
    }
    if (currentProbe === 'conflict') {
      return { ok: false, code: 'socket_conflict', message: 'A non-socket file exists at .runtime/ai4mathd.sock.' };
    }
    if (!daemonBin.ready) {
      return { ok: false, code: 'missing_binary', message: 'The ai4mathd binary was not found. Build target/debug/ai4mathd or configure daemonPath.' };
    }
    if (managed && managed.projectId !== project.id) {
      await stopManagedDaemon();
    }
    if (currentProbe === 'stale') {
      // Confirmed ECONNREFUSED on a real Unix socket file -> safe to remove stale node.
      try {
        const stat = await lstat(project.socket_path);
        if (stat.isSocket()) await unlink(project.socket_path);
      } catch {
        return { ok: false, code: 'stale_socket', message: 'Could not remove stale ai4mathd.sock before starting.' };
      }
    }
    const logs = [];
    const child = spawn(daemonBin.path, ['--project', project.root, '--tick-ms', String(tickMs)], {
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let spawnError = null;
    child.once('error', (err) => { spawnError = err; appendLog(logs, 'spawn error: ' + err.message); });
    child.stdout?.on('data', (chunk) => appendLog(logs, chunk));
    child.stderr?.on('data', (chunk) => appendLog(logs, chunk));
    const startedAt = new Date().toISOString();
    const closed = new Promise((resolve) => {
      child.once('close', (code, signal) => {
        const record = { project_id: project.id, code, signal, at: new Date().toISOString(), logs: logs.slice(-20) };
        lastExit = record;
        if (managed?.child === child) managed = null;
        resolve(record);
      });
    });
    managed = {
      projectId: project.id,
      root: project.root,
      socketPath: project.socket_path,
      child,
      pid: child.pid ?? null,
      startedAt,
      logs,
      exiting: false,
      closed,
    };

    // Wait up to 3.5s for the daemon socket to accept connections.
    const deadline = Date.now() + 3500;
    while (Date.now() < deadline) {
      if (spawnError || child.exitCode !== null || child.signalCode !== null) break;
      if (await probeSocket(project.socket_path, 250) === 'live') {
        bridge.retarget(project.socket_path);
        return { ok: true, mode: 'managed', pid: child.pid ?? null };
      }
      await delay(80);
    }
    if (spawnError || child.exitCode !== null || child.signalCode !== null) {
      return {
        ok: false,
        code: 'daemon_exited',
        message: logs.at(-1) ?? 'ai4mathd exited before opening its Unix socket.',
        logs: logs.slice(-12),
      };
    }
    // Still starting; retarget anyway so bridge connects as soon as socket opens.
    bridge.retarget(project.socket_path);
    return { ok: true, mode: 'managed', pid: child.pid ?? null, starting: true };
  }

  async function buildState() {
    const activeSocket = bridge.socketPath() ?? null;
    const projectStates = await Promise.all(registry.projects.map(async (project) => {
      const exists = existsSync(project.root);
      const initialized = exists && isInitializedProject(project.root);
      const length = checkSocketPathLength(project.root);
      const probe = exists && length.ok ? await probeSocket(project.socket_path) : (length.ok ? 'stopped' : 'invalid');
      const isManaged = managed !== null && managed.projectId === project.id && managed.child.exitCode === null;
      const daemon_status = probe === 'live' ? (isManaged ? 'managed' : 'external')
        : isManaged ? 'starting'
        : probe;
      return {
        id: project.id,
        name: project.name,
        root: project.root,
        engine: (exists && readEngineMarker(project.root)) || project.engine,
        socket_path: project.socket_path,
        socket_bytes: length.bytes,
        socket_limit: length.limit,
        exists,
        initialized,
        managed_git: project.managed_git ?? readManagedGit(project.root),
        daemon_status,
        active: activeSocket === project.socket_path,
        updated_at: project.updated_at,
      };
    }));

    // If the currently bound socket comes from config/env and isn't in the saved list yet,
    // expose it transparently as an unmanaged entry so the UI always reflects ground truth.
    let activeProjectId = projectStates.find((p) => p.active)?.id ?? null;
    let unmanagedActive = null;
    if (activeSocket && !activeProjectId) {
      const inferredRoot = inferProjectRootFromSocket(activeSocket);
      const probe = await probeSocket(activeSocket);
      unmanagedActive = {
        root: inferredRoot,
        socket_path: activeSocket,
        engine: inferredRoot ? readEngineMarker(inferredRoot) : null,
        managed_git: inferredRoot ? readManagedGit(inferredRoot) : null,
        daemon_status: probe === 'live' ? 'external' : probe,
      };
    }

    const chiefPreset = typeof rolePresets?.chief === 'string' && rolePresets.chief.length > 0
      ? rolePresets.chief
      : defaultChiefPreset;

    return {
      active_project_id: activeProjectId,
      active_socket_path: activeSocket,
      unmanaged_active: unmanagedActive,
      projects: projectStates,
      binaries: {
        cli_ready: cli.ready,
        daemon_ready: daemonBin.ready,
        cli_path: cli.path,
        daemon_path: daemonBin.path,
      },
      socket_limit_bytes: MAX_SOCKET_BYTES,
      chief_preset: chiefPreset,
      chief_bound: typeof rolePresets?.chief === 'string' && rolePresets.chief.length > 0,
      role_presets: { ...rolePresets },
      proof_slots: {
        prover: Array.isArray(proofSlots?.prover) ? proofSlots.prover.slice() : [],
        automation: Array.isArray(proofSlots?.automation) ? proofSlots.automation.slice() : [],
      },
      managed_daemon: managed ? {
        project_id: managed.projectId,
        pid: managed.pid,
        started_at: managed.startedAt,
        logs: managed.logs.slice(-30),
      } : null,
      last_daemon_exit: lastExit,
    };
  }

  return Object.freeze({
    async state() {
      return buildState();
    },

    async projectInit({ root, engine = 'lean4', name, auto_start = true, setupManagedGit = true } = {}) {
      return serialized(async () => {
        if (typeof root !== 'string' || !isAbsolute(root) || root.includes('\0') || /[\u0000-\u001f\u007f]/.test(root)) {
          throw new BridgeError('invalid');
        }
        if (!ENGINES.has(engine)) throw new BridgeError('invalid');
        let canonical;
        try {
          if (!statSync(root).isDirectory()) {
            return { ok: false, code: 'not_directory', message: 'Selected path is not an existing directory.', state: await buildState() };
          }
          canonical = realpathSync(root);
        } catch {
          return { ok: false, code: 'missing_directory', message: 'Selected directory does not exist.', state: await buildState() };
        }
        const lengthCheck = checkSocketPathLength(canonical);
        if (!lengthCheck.ok) {
          return {
            ok: false,
            code: 'socket_path_too_long',
            message: `Unix socket path is ${lengthCheck.bytes} bytes (platform limit ${lengthCheck.limit}). Please pick a shorter directory path.`,
            state: await buildState(),
          };
        }

        let resolvedEngine = engine;
        let initializedNow = false;
        if (isInitializedProject(canonical)) {
          resolvedEngine = readEngineMarker(canonical) ?? engine;
        } else {
          if (!cli.ready) {
            return {
              ok: false,
              code: 'missing_binary',
              message: 'The ai4math CLI binary was not found. Build target/debug/ai4math or configure cliPath.',
              state: await buildState(),
            };
          }
          try {
            await execFileAsync(cli.path, [setupManagedGit ? 'init-managed' : 'init', canonical, engine], {
              timeout: 30_000,
              maxBuffer: 256 * 1024,
            });
            initializedNow = true;
          } catch (error) {
            const detail = sanitizeLine(error?.stderr || error?.message || 'ai4math init failed');
            return { ok: false, code: setupManagedGit ? 'managed_git_failed' : 'init_failed', message: detail || 'ai4math init failed.', state: await buildState() };
          }
        }

        const now = new Date().toISOString();
        const id = projectIdFor(canonical);
        const displayName = typeof name === 'string' && name.trim().length > 0
          ? name.trim().slice(0, 80) : basename(canonical) || 'math-project';
        const existingIndex = registry.projects.findIndex((p) => p.root === canonical);
        const record = {
          id,
          name: displayName,
          root: canonical,
          engine: resolvedEngine,
          socket_path: lengthCheck.socketPath,
          created_at: existingIndex >= 0 ? registry.projects[existingIndex].created_at : now,
          updated_at: now,
          managed_git: readManagedGit(canonical),
        };
        if (existingIndex >= 0) {
          registry.projects[existingIndex] = record;
        } else {
          if (registry.projects.length >= MAX_PROJECTS) {
            return { ok: false, code: 'limit_reached', message: `Project list is limited to ${MAX_PROJECTS} entries.`, state: await buildState() };
          }
          registry.projects.unshift(record);
        }
        registry.active_project_id = record.id;
        saveRegistry(storePath, registry);

        if (setupManagedGit && !record.managed_git && cli.ready) {
          try {
            const observed = JSON.parse((await execFileAsync(cli.path, ['status', canonical], {
              timeout: 10_000, maxBuffer: 256 * 1024,
            })).stdout);
            if (observed.managed_repo_bound !== true) {
              await execFileAsync(cli.path, ['init-managed', canonical, resolvedEngine], {
                timeout: 30_000, maxBuffer: 256 * 1024,
              });
            }
            record.managed_git = observed.managed_repo_bound === true
              ? normalizeManagedGit({ bound: true, repo: observed.managed_repo })
              : readManagedGit(canonical);
            const existing = registry.projects.find((p) => p.root === canonical);
            if (existing) existing.managed_git = record.managed_git;
            saveRegistry(storePath, registry);
          } catch (error) {
            const detail = sanitizeLine(error?.stderr || error?.message || 'managed Git setup failed');
            return { ok: false, code: 'managed_git_failed', message: detail || 'managed Git setup failed.', state: await buildState() };
          }
        }

        let daemonResult = null;
        if (auto_start) {
          daemonResult = await startManagedForProject(record);
        } else {
          bridge.retarget(record.socket_path);
        }

        return {
          ok: daemonResult ? daemonResult.ok : true,
          code: daemonResult && !daemonResult.ok ? daemonResult.code : undefined,
          message: daemonResult && !daemonResult.ok ? daemonResult.message : undefined,
          initialized: initializedNow,
          managed_git: readManagedGit(canonical),
          project: record,
          state: await buildState(),
        };
      });
    },

    async projectSelect({ project_id, auto_start = false } = {}) {
      return serialized(async () => {
        const project = registry.projects.find((p) => p.id === project_id);
        if (!project) return { ok: false, code: 'not_found', message: 'Project not found in Workbench list.', state: await buildState() };
        const lengthCheck = checkSocketPathLength(project.root);
        if (!lengthCheck.ok) {
          return {
            ok: false,
            code: 'socket_path_too_long',
            message: `Unix socket path is ${lengthCheck.bytes} bytes (limit ${lengthCheck.limit}).`,
            state: await buildState(),
          };
        }
        if (managed && managed.projectId !== project.id) {
          await stopManagedDaemon();
        }
        project.updated_at = new Date().toISOString();
        registry.active_project_id = project.id;
        saveRegistry(storePath, registry);
        let daemonResult = null;
        if (auto_start) {
          daemonResult = await startManagedForProject(project);
        } else {
          bridge.retarget(project.socket_path);
        }
        return {
          ok: daemonResult ? daemonResult.ok : true,
          code: daemonResult && !daemonResult.ok ? daemonResult.code : undefined,
          message: daemonResult && !daemonResult.ok ? daemonResult.message : undefined,
          project,
          state: await buildState(),
        };
      });
    },

    async projectRemove({ project_id } = {}) {
      return serialized(async () => {
        const index = registry.projects.findIndex((p) => p.id === project_id);
        if (index < 0) return { ok: false, code: 'not_found', message: 'Project not found.', state: await buildState() };
        const [removed] = registry.projects.splice(index, 1);
        if (managed && managed.projectId === removed.id) {
          await stopManagedDaemon();
        }
        if (registry.active_project_id === removed.id) {
          const next = registry.projects[0] ?? null;
          registry.active_project_id = next ? next.id : null;
          bridge.retarget(next ? next.socket_path : undefined);
        }
        saveRegistry(storePath, registry);
        return { ok: true, removed_id: removed.id, state: await buildState() };
      });
    },

    async daemonStart({ project_id, tick_ms = 2000 } = {}) {
      return serialized(async () => {
        const project = findProject(project_id);
        if (!project) return { ok: false, code: 'no_project', message: 'No active AI4Math project selected.', state: await buildState() };
        if (registry.active_project_id !== project.id) {
          registry.active_project_id = project.id;
          saveRegistry(storePath, registry);
        }
        const outcome = await startManagedForProject(project, tick_ms);
        return { ...outcome, state: await buildState() };
      });
    },

    async daemonStop({ project_id } = {}) {
      return serialized(async () => {
        const targetId = project_id ?? managed?.projectId ?? registry.active_project_id;
        if (!managed || (targetId && managed.projectId !== targetId)) {
          return {
            ok: false,
            code: 'external_or_stopped',
            message: 'No Host-managed daemon is running for this project. External daemons are never killed by DSH.',
            state: await buildState(),
          };
        }
        const outcome = await stopManagedDaemon();
        return { ok: outcome.stopped, ...outcome, state: await buildState() };
      });
    },

    async close() {
      await serialized(async () => {
        await stopManagedDaemon();
      });
    },
  });
}
