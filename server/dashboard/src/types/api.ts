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
  /** Why a successful operation produced nothing: `no_memories`, `provider_error`,
   *  `unparsed_answer`, `embed_failed`, `add_failed`. Null for the ordinary case. */
  reason?: string | null;
  /** The stashed add behind this row, when the input was kept for a retry. */
  failed_add_id?: number | null;
}

/** One ADD that stored nothing and kept its input, so it can be retried. */
export interface ApiFailedAdd {
  id: number;
  reason: string;
  user_id: string | null;
  agent_id: string | null;
  run_id: string | null;
  metadata: Record<string, unknown> | null;
  messages: { role: string; content: string }[];
  /** Entries extraction produced (only when an embedding failure lost them). */
  items: string[];
  infer: boolean;
  memory_type: string | null;
  expiration_date: string | null;
  attempts: number;
  last_error: string | null;
  created_at: string;
  last_attempt_at: string;
}

export interface ApiFailedAdds {
  items: ApiFailedAdd[];
  total: number;
}

export interface ApiRetryResult {
  retried: number;
  stored: number;
  failed: number;
  remaining: number;
  stopped_reason: string | null;
}

/** One line of the application log (JSONL in the data directory). */
export interface ApiAppLogLine {
  ts: string;
  level: "info" | "warn" | "error";
  target: string;
  msg: string;
  fields?: Record<string, unknown>;
}

export type EntityType = "user" | "agent" | "run";

export interface Entity {
  id: string;
  type: EntityType;
  total_memories: number;
  created_at: string | null;
  updated_at: string | null;
}
