// Prefer the native server, but keep the TypeScript implementation as a safety net while the native
// implementation is being brought up on each platform.
import { spawn } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = dirname(fileURLToPath(import.meta.url));
const ENV_URL = (process.env.MAKEFUN_ENV_URL ?? '').replace(/\/+$/, '');
const REQUEST_TIMEOUT_MS = 30_000;
/** How long the native binary has to report itself registered before the TS server takes over. */
const NATIVE_BOOT_TIMEOUT_MS = 8_000;
const RETRY_INITIAL_MS = 250;
const RETRY_MAX_MS = 2_000;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// A forge that dies mid-trial scores 0, so a stray exception is logged, never fatal.
process.on('uncaughtException', (err) => {
  process.stderr.write(`[entry] uncaught exception (kept serving): ${err.stack ?? err.message}\n`);
});
process.on('unhandledRejection', (reason) => {
  process.stderr.write(`[entry] unhandled rejection (kept serving): ${reason instanceof Error ? (reason.stack ?? reason.message) : String(reason)}\n`);
});

function retryDelay(attempt) {
  return Math.min(RETRY_MAX_MS, RETRY_INITIAL_MS * 2 ** Math.min(attempt, 8));
}

function runNative() {
  const binary = join(ROOT, 'bin', process.platform === 'darwin' ? 'forge-darwin-arm64' : 'forge-linux-x86_64');

  return new Promise((resolve) => {
    let registered = false;
    let settled = false;
    let pendingLine = '';
    let exitCode = null;

    const child = spawn(binary, [], {
      cwd: ROOT,
      env: process.env,
      stdio: ['ignore', 'pipe', 'pipe'],
    });

    const finish = (result) => {
      if (settled) return;
      settled = true;
      clearTimeout(deadline);
      resolve(result);
    };

    child.stdout.on('data', (chunk) => {
      process.stdout.write(chunk);
      pendingLine += chunk.toString();
      const lines = pendingLine.split(/\r?\n/);
      pendingLine = lines.pop() ?? '';
      if (!registered && lines.includes('[forge] registered')) {
        registered = true;
        clearTimeout(deadline);
      }
    });
    child.stderr.on('data', (chunk) => process.stderr.write(chunk));

    const deadline = setTimeout(() => {
      if (registered) return;
      process.stderr.write('[entry] native server did not register within 30 seconds; falling back to TypeScript\n');
      child.kill('SIGKILL');
    }, NATIVE_BOOT_TIMEOUT_MS);

    child.on('error', (err) => {
      process.stderr.write(`[entry] could not start native server: ${err.message}\n`);
      finish({ registered: false, code: 1 });
    });
    child.on('close', (code, signal) => {
      exitCode = code;
      finish({ registered, code: exitCode ?? (signal ? 1 : 0) });
    });
  });
}

/** POST the bound port until the env accepts it. 200 is normal; 409 means another register won. */
async function register(port) {
  for (let attempt = 0; ; attempt += 1) {
    try {
      const res = await fetch(`${ENV_URL}/register`, {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ port }),
        signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
      });
      if (res.status === 200 || res.status === 409) return;
      if (res.status === 400 || res.status === 405) {
        throw new Error(`register answered HTTP ${res.status}: ${await res.text()}`);
      }
    } catch (err) {
      if (err instanceof Error && err.message.startsWith('register answered HTTP ')) throw err;
    }
    await sleep(retryDelay(attempt));
  }
}

/** Poll GET /done until it answers {"done":true}; never return before that. */
async function waitForDone() {
  for (let attempt = 0; ; attempt += 1) {
    try {
      const res = await fetch(`${ENV_URL}/done`, { signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS) });
      if (res.ok) {
        const body = await res.json();
        if (body.done === true) return;
      }
    } catch {
      // The env is not answering yet, or the poll timed out: it is not done, so poll again.
    }
    await sleep(retryDelay(attempt));
  }
}

async function runTypeScriptFallback() {
  if (ENV_URL === '') {
    process.stderr.write('[entry] MAKEFUN_ENV_URL is not set: there is no env to register with\n');
    process.exit(1);
  }

  const { createApp } = await import('./app.ts');
  const server = createApp();
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, () => resolve());
  });
  const address = server.address();
  if (address === null || typeof address === 'string') throw new Error('the server did not bind an ephemeral TCP port');
  process.stdout.write(`[entry] listening on port ${address.port}\n`);

  await register(address.port);
  process.stdout.write('[entry] registered; waiting for the env to say done\n');
  await waitForDone();
  server.close();
  process.stdout.write('[entry] done\n');
}

try {
  const native = await runNative();
  if (native.registered) process.exit(native.code);
  await runTypeScriptFallback();
  process.exit(0);
} catch (err) {
  process.stderr.write(`[entry] ${err instanceof Error ? err.message : String(err)}\n`);
  process.exit(1);
}
