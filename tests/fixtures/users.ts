// User repository helpers used by the account service.
import { Database } from "./database";

const db = new Database("accounts");

export interface User {
  id: string;
  name: string;
  email: string;
  createdAt: Date;
  disabled: boolean;
}

export function getUserById(id: string): User | undefined {
  const row = db.queryOne(
    "SELECT id, name, email, created_at, disabled FROM users WHERE id = ?",
    [id],
  );
  if (!row) {
    return undefined;
  }
  return {
    id: row.id,
    name: row.name,
    email: row.email,
    createdAt: new Date(row.created_at),
    disabled: row.disabled === 1,
  };
}

export function listActiveUsers(limit: number, offset: number): User[] {
  const rows = db.queryAll(
    "SELECT id, name, email, created_at, disabled FROM users WHERE disabled = 0 ORDER BY name LIMIT ? OFFSET ?",
    [limit, offset],
  );
  return rows.map((row) => ({
    id: row.id,
    name: row.name,
    email: row.email,
    createdAt: new Date(row.created_at),
    disabled: false,
  }));
}

export function renameAccount(id: string, name: string): void {
  if (name.trim().length === 0) {
    throw new Error("A name must not be blank.");
  }
  db.execute("UPDATE users SET name = ? WHERE id = ?", [name.trim(), id]);
  db.execute("INSERT INTO audit_log (entity, entity_id, action) VALUES ('users', ?, 'rename')", [id]);
  db.execute("DELETE FROM session_cache WHERE owner = ?", [id]);
}
