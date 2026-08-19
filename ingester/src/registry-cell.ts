// RegistryCell — the (project_id, key, active) set the Worker authenticates
// against. One cell for the whole fleet.
//
// nashcode owns it: it pushes the whole set with PUT and reads it back with GET.
// The keys in here are DSN public keys, which are routing identifiers rather
// than secrets — they travel in browser query strings by design. The thing worth
// protecting is the bearer token that lets you write this set, and that never
// reaches this class.

import { normaliseKey } from "./protocol";

export interface RegistryEntry {
  project_id: string;
  key: string;
  active: boolean;
}

export class RegistryCell {
  private sql: any;

  constructor(state: any, _env: unknown) {
    this.sql = state.storage.sql;
    this.sql.exec(
      `CREATE TABLE IF NOT EXISTS projects (
         project_id TEXT PRIMARY KEY,
         key TEXT NOT NULL,
         active INTEGER NOT NULL
       )`,
    );
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname !== "/registry") return json(404, { detail: "no such cell route" });
    if (request.method === "GET") return json(200, { projects: this.read() });
    if (request.method === "PUT") return this.replace(request);
    return json(405, { detail: "the registry takes GET and PUT" });
  }

  private read(): RegistryEntry[] {
    const out: RegistryEntry[] = [];
    for (const row of this.sql.exec(`SELECT project_id, key, active FROM projects ORDER BY project_id`)) {
      out.push({ project_id: String(row.project_id), key: String(row.key), active: Number(row.active) === 1 });
    }
    return out;
  }

  /// PUT replaces the set rather than merging it. A project nashcode has deleted
  /// has to stop authenticating here, and a merge would leave it working for
  /// ever.
  private async replace(request: Request): Promise<Response> {
    let payload: any;
    try {
      payload = await request.json();
    } catch {
      return json(400, { detail: "the registry body is not JSON" });
    }
    const raw = payload?.projects;
    if (!Array.isArray(raw)) return json(400, { detail: "the registry body needs a projects array" });

    const entries: RegistryEntry[] = [];
    for (const item of raw) {
      const projectId = String(item?.project_id ?? "");
      const key = normaliseKey(item?.key);
      if (!/^[0-9]{1,19}$/.test(projectId)) {
        return json(400, { detail: `project_id ${JSON.stringify(projectId)} is not a numeric id` });
      }
      if (!key) return json(400, { detail: `project ${projectId} has no 32-hex key` });
      entries.push({ project_id: projectId, key, active: item?.active !== false });
    }

    this.sql.exec(`DELETE FROM projects`);
    for (const entry of entries) {
      this.sql.exec(
        `INSERT INTO projects (project_id, key, active) VALUES (?, ?, ?)`,
        entry.project_id,
        entry.key,
        entry.active ? 1 : 0,
      );
    }
    return json(200, { projects: entries.length });
  }
}

function json(status: number, payload: unknown): Response {
  return new Response(JSON.stringify(payload), {
    status,
    headers: { "content-type": "application/json" },
  });
}
