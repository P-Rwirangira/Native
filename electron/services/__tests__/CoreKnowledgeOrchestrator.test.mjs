import { afterEach, test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import Database from 'better-sqlite3';

const root = path.resolve(import.meta.dirname, '../../..');
const { CoreKnowledgeDatabaseManager } = await import(pathToFileURL(path.join(root, 'dist-electron/electron/knowledge/CoreKnowledgeDatabaseManager.js')).href);
const { CoreKnowledgeOrchestrator } = await import(pathToFileURL(path.join(root, 'dist-electron/electron/knowledge/CoreKnowledgeOrchestrator.js')).href);
const { DocType } = await import(pathToFileURL(path.join(root, 'dist-electron/electron/knowledge/types.js')).href);

const tempPaths = [];
afterEach(() => {
  while (tempPaths.length) fs.rmSync(tempPaths.pop(), { force: true });
});

test('core profile context persists a resume and injects grounded evidence', async () => {
  const db = new Database(':memory:');
  const orchestrator = new CoreKnowledgeOrchestrator(new CoreKnowledgeDatabaseManager(db));
  const file = path.join(os.tmpdir(), `natively-core-profile-${process.pid}-${Date.now()}.txt`);
  tempPaths.push(file);
  fs.writeFileSync(file, 'Sarah Chen\nSenior Software Engineer\nSkills: TypeScript, Rust, SQL\nBuilt a payment platform at Acme.', 'utf8');

  const result = await orchestrator.ingestDocument(file, DocType.RESUME);
  assert.equal(result.success, true);
  assert.equal(orchestrator.getStatus().hasResume, true);
  assert.equal(orchestrator.getProfileData().identity.name, 'Sarah Chen');

  orchestrator.setKnowledgeMode(true);
  const answerContext = await orchestrator.processQuestion('What are my skills?');
  assert.equal(answerContext.factualRecall, true);
  assert.match(answerContext.contextBlock, /TypeScript, Rust, SQL/);
  assert.match(answerContext.contextBlock, /Never invent employers, dates, metrics, skills, or qualifications/);

  const restored = new CoreKnowledgeOrchestrator(new CoreKnowledgeDatabaseManager(db));
  assert.equal(restored.getStatus().hasResume, true);
  db.close();
});
