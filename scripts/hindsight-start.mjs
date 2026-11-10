// Cross-platform Hindsight launcher. Windows cannot rely on bash/WSL, so this
// performs the same provider-router setup as hindsight-start.sh and keeps the
// Python child attached for clean process-tree shutdown.
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { config as loadDotenv } from 'dotenv';
import { buildHindsightRouterConfig } from './hindsight-llm-config.mjs';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
loadDotenv({ path: path.join(root, '.env'), override: false, quiet: true });

const router = buildHindsightRouterConfig(process.env);
const env = { ...process.env, PYTHONUNBUFFERED: '1' };
if (router) env.HINDSIGHT_API_LLM_LITELLMROUTER_CONFIG = JSON.stringify(router);

const candidates = [process.env.HINDSIGHT_PYTHON, 'python', 'python3'].filter(Boolean);
const serverScript = path.join(root, 'scripts', 'hindsight-dev-server.py');
if (!fs.existsSync(serverScript)) throw new Error(`Hindsight server script not found: ${serverScript}`);

let child;
for (const executable of candidates) {
  try {
    child = spawn(executable, [serverScript], { cwd: root, env, stdio: 'inherit', windowsHide: true });
    await new Promise((resolve, reject) => {
      child.once('spawn', resolve);
      child.once('error', reject);
    });
    break;
  } catch {
    child = undefined;
  }
}
if (!child) throw new Error('No Python executable could start Hindsight. Set HINDSIGHT_PYTHON.');

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => { try { child.kill(signal); } catch { /* child already stopped */ } });
}
const code = await new Promise((resolve) => child.once('exit', (exitCode) => resolve(exitCode ?? 1)));
process.exitCode = Number(code);
