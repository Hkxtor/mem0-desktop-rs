import type { KeyPoolStatus } from "@/utils/api-endpoints";

type ProviderConfig = {
  provider?: string;
  config?: {
    model?: string;
    api_key?: string;
    embedding_dims?: number;
    [key: string]: unknown;
  };
};
export type EffectiveConfig = {
  llm?: ProviderConfig;
  embedder?: ProviderConfig;
  /** Optional classifier that tags memories with the preset categories. */
  systemone?: ProviderConfig;
  vector_store?: { config?: { embedding_model_dims?: number } };
};

/**
 * The embedding width each bundled provider produces by default.
 *
 * The DuckDB table stores vectors in a fixed-width column, so switching provider without switching
 * dimensions writes 768 numbers into a 1536-wide column and fails at the first insert. These are
 * only defaults for the form; whatever the operator types is what gets sent.
 */
export const DEFAULT_EMBEDDING_DIMS: Record<string, number> = {
  openai: 1536,
  gemini: 768,
};

/**
 * Base URL key per provider.
 *
 * mem0 reads a provider-specific key (`openai_base_url`, `anthropic_base_url`, `gemini_base_url`)
 * rather than one shared `base_url`, so the form cannot send a single fixed name.
 */
/**
 * Example model per provider, for the empty field's placeholder.
 *
 * A field that says `text-embedding-3-small` while the provider is Gemini invites a
 * model name from the wrong vendor. Gemini names also carry a publisher (`models/…`);
 * the embedder qualifies a bare one, but showing the qualified form is what the docs use.
 */
export const EXAMPLE_MODEL: Record<string, string> = {
  openai: "gpt-4.1-nano-2025-04-14",
  anthropic: "claude-sonnet-4-5",
  gemini: "models/gemini-2.5-flash",
  ollama: "llama3.1",
};

export const EXAMPLE_EMBEDDER_MODEL: Record<string, string> = {
  openai: "text-embedding-3-small",
  gemini: "models/gemini-embedding-001",
  ollama: "nomic-embed-text",
};

export const baseUrlKey = (provider: string) => `${provider}_base_url`;

export const getEffectiveConfig = (data: unknown): EffectiveConfig | null => {
  if (!data || typeof data !== "object") {
    return null;
  }

  const record = data as Record<string, unknown>;
  return (
    (record.effective_config as EffectiveConfig) ||
    (record.config as EffectiveConfig) ||
    (record as EffectiveConfig)
  );
};

/** The base URL currently configured for a provider, or "" when it points at the default. */
export const readBaseUrl = (section: ProviderConfig | undefined): string => {
  const provider = section?.provider;
  if (!provider) {
    return "";
  }
  // Chat providers keep their endpoint under `{provider}_base_url` so switching provider
  // cannot carry the old host over. The SystemOne decision backend has its own protocol
  // and a single `base_url`, so that is the fallback.
  const value =
    section?.config?.[baseUrlKey(provider)] ?? section?.config?.base_url;
  return typeof value === "string" ? value : "";
};

/**
 * The key the embedder should use.
 *
 * The two sections are configured separately, so a key typed into the embedder's own field always
 * wins. When the two share a provider the LLM's key is a reasonable fallback — one OpenAI key serves
 * both — but only then: handing an Anthropic key to Gemini is a 401, not a convenience.
 */
/** What the embedder's key field says it will do, given the two providers. */
export const embedderKeyHint = ({
  serverHasKey,
  embedderProvider,
  llmProvider,
}: {
  serverHasKey: boolean;
  embedderProvider?: string;
  llmProvider?: string;
}) => {
  if (serverHasKey) return "留空表示沿用已保存的密钥。";
  if (embedderProvider && embedderProvider === llmProvider) {
    return "留空表示复用 LLM 的密钥。";
  }
  return "必填：这个 provider 不复用 LLM 的密钥。";
};

/**
 * Read a multi-line key field into the list the server stores.
 *
 * One key per line, blanks dropped, and a repeat removed: the same key twice is the same quota
 * twice, which only makes the rotation spend it twice as often. The values are trimmed because a
 * pasted list arrives with indentation.
 */
export const parseApiKeys = (raw: string): string[] => {
  const seen: string[] = [];
  for (const line of raw.split("\n")) {
    const key = line.trim();
    if (key && !seen.includes(key)) {
      seen.push(key);
    }
  }
  return seen;
};

/**
 * The one line under a key field: how many keys this section can spend, whether one is parked,
 * whether one was rejected. Renders nothing when the section has no pool of its own to report.
 */
export const keyPoolSummary = (status?: KeyPoolStatus): string | null => {
  if (!status) return null;
  if (status.inherited) {
    return status.keys > 0 ? `沿用 LLM 的 ${status.keys} 把密钥` : null;
  }
  if (status.keys === 0) return null;
  const parts = [`${status.keys} 把`];
  if (status.cooling > 0) {
    parts.push(
      status.next_available_in > 0
        ? `${status.cooling} 把冷却中（${status.next_available_in}s）`
        : `${status.cooling} 把冷却中`,
    );
  }
  if (status.revoked > 0) parts.push(`${status.revoked} 把已作废`);
  return parts.join(" · ");
};

export const buildProviderConfig = ({
  provider,
  model,
  apiKeys,
  baseUrl,
  embeddingDims,
}: {
  provider: string;
  model: string;
  /** The typed keys. Empty means "change nothing", so the stored pool survives a save. */
  apiKeys?: string[];
  baseUrl?: string;
  embeddingDims?: number;
}) => {
  if (!provider) {
    return undefined;
  }

  const config: Record<string, unknown> = {
    model: model || undefined,
    api_keys: apiKeys && apiKeys.length > 0 ? apiKeys : undefined,
  };

  // Only sent when the operator filled it in: an empty string would otherwise overwrite a value
  // that is set elsewhere, including one coming from the environment.
  if (baseUrl?.trim()) {
    config[baseUrlKey(provider)] = baseUrl.trim();
  }

  if (embeddingDims && Number.isFinite(embeddingDims)) {
    config.embedding_dims = embeddingDims;
  }

  return { provider, config };
};
