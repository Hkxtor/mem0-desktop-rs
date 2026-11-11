export interface Memory {
  id: string;
  memory: string;
  user_id?: string;
  agent_id?: string;
  run_id?: string;
  created_at?: string;
  updated_at?: string;
  categories?: string[];
  expiration_date?: string | null;
  metadata?: Record<string, unknown>;
}

/** One message a memory was extracted from (`GET /memories/{id}/inputs`). */
export interface MemoryInput {
  seq: number;
  role?: string | null;
  content: string;
  created_at?: string;
}

/** One row of `GET /memories/{id}/history`: ADD / UPDATE / DELETE. */
export interface MemoryHistoryEntry {
  id: string;
  memory_id: string;
  old_memory?: string | null;
  new_memory?: string | null;
  event: string;
  created_at?: string | null;
  updated_at?: string | null;
  is_deleted?: boolean;
  actor_id?: string | null;
  role?: string | null;
}

export interface ApiKey {
  id: string;
  label: string;
  key_prefix: string;
  created_at: string;
  last_used_at: string | null;
}

export interface ApiKeyCreateResponse {
  id: string;
  label: string;
  key: string;
  key_prefix: string;
  created_at: string;
}

/**
 * One memory operation, as the Requests page lists them.
 *
 * `operation` is ADD / SEARCH / LIST / GET / UPDATE / DELETE / DELETE_ALL / RESET /
 * IMPORT. A backend that only reports HTTP traffic answers with method/path instead,
 * which the page still renders.
 */
export interface ApiRequestLog {
  id: string;
  created_at: string;
  operation?: string;
  detail?: string;
  results?: number | null;
  status_code: number;
  latency_ms: number;
  method?: string;
  path?: string;
  auth_type?: string;
}

export type EntityType = "user" | "agent" | "run";

export interface Entity {
  id: string;
  type: EntityType;
  total_memories: number;
  created_at: string | null;
  updated_at: string | null;
}
