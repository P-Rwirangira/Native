import type { ProfileDocType } from './types';

export interface CoreKnowledgeDocument {
  id: number;
  doc_type: ProfileDocType;
  source_uri: string;
  raw_text: string;
  structured_data: Record<string, unknown>;
  created_at: string;
  updated_at: string;
}

/** Durable store for the open-source profile-context implementation. */
export class CoreKnowledgeDatabaseManager {
  constructor(private readonly db: any) {
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS core_profile_documents (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        doc_type TEXT NOT NULL CHECK (doc_type IN ('resume', 'jd')),
        source_uri TEXT NOT NULL,
        raw_text TEXT NOT NULL,
        structured_json TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
      );
      CREATE INDEX IF NOT EXISTS idx_core_profile_documents_type
        ON core_profile_documents(doc_type, id DESC);
    `);
  }

  getActive(docType: ProfileDocType): CoreKnowledgeDocument | null {
    const row = this.db.prepare(`
      SELECT id, doc_type, source_uri, raw_text, structured_json, created_at, updated_at
      FROM core_profile_documents WHERE doc_type = ? ORDER BY id DESC LIMIT 1
    `).get(docType) as any;
    return row ? this.mapRow(row) : null;
  }

  replace(docType: ProfileDocType, sourceUri: string, rawText: string, structured: Record<string, unknown>): CoreKnowledgeDocument {
    const save = this.db.transaction(() => {
      this.db.prepare('DELETE FROM core_profile_documents WHERE doc_type = ?').run(docType);
      const result = this.db.prepare(`
        INSERT INTO core_profile_documents (doc_type, source_uri, raw_text, structured_json)
        VALUES (?, ?, ?, ?)
      `).run(docType, sourceUri, rawText, JSON.stringify(structured));
      return Number(result.lastInsertRowid);
    });
    const row = this.db.prepare(`
      SELECT id, doc_type, source_uri, raw_text, structured_json, created_at, updated_at
      FROM core_profile_documents WHERE id = ?
    `).get(save()) as any;
    return this.mapRow(row);
  }

  deleteByType(docType: ProfileDocType): void {
    this.db.prepare('DELETE FROM core_profile_documents WHERE doc_type = ?').run(docType);
  }

  private mapRow(row: any): CoreKnowledgeDocument {
    let structured_data: Record<string, unknown> = {};
    try { structured_data = JSON.parse(row.structured_json); } catch { /* retain safe empty object */ }
    return { ...row, structured_data };
  }
}
