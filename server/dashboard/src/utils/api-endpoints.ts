export const AUTH_ENDPOINTS = {
  SETUP_STATUS: "/auth/setup-status",
  REGISTER: "/auth/register",
  LOGIN: "/auth/login",
  REFRESH: "/auth/refresh",
  ME: "/auth/me",
  CHANGE_PASSWORD: "/auth/change-password",
  ONBOARDING_COMPLETE: "/auth/onboarding-complete",
} as const;

export const MEMORY_ENDPOINTS = {
  BASE: "/memories",
  BY_ID: (memoryId: string) => `/memories/${memoryId}`,
  HISTORY: (memoryId: string) => `/memories/${memoryId}/history`,
  INPUTS: (memoryId: string) => `/memories/${memoryId}/inputs`,
  CONFIGURE: "/configure",
  CONFIGURE_PROVIDERS: "/configure/providers",
  RESET: "/reset",
  GENERATE_INSTRUCTIONS: "/generate-instructions",
} as const;

export const API_KEY_ENDPOINTS = {
  BASE: "/api-keys",
  BY_ID: (keyId: string) => `/api-keys/${keyId}`,
} as const;

export const SERVER_ENDPOINTS = {
  SETTINGS: "/settings/server",
  DESKTOP: "/settings/desktop",
} as const;

export const SYSTEMONE_ENDPOINTS = {
  CATEGORIES: "/systemone/categories",
  COUNTS: "/systemone/counts",
  CLASSIFY: "/systemone/classify",
  CLASSIFY_MEMORY: "/systemone/classify-memory",
  CLASSIFY_BATCH: "/systemone/classify-batch",
  TEST: "/systemone/test",
} as const;

export const DIAGNOSTICS_ENDPOINTS = {
  STORAGE: "/diagnostics/storage",
} as const;

export const REQUEST_ENDPOINTS = {
  BASE: "/requests",
} as const;

export const LOG_ENDPOINTS = {
  APP: "/logs/app",
  REQUESTS: "/logs/requests",
} as const;

export const EXPORT_ENDPOINTS = {
  EXPORT: "/export",
  IMPORT: "/import",
} as const;

export const ENTITY_ENDPOINTS = {
  BASE: "/entities",
  BY_ID: (type: string, id: string) =>
    `/entities/${type}/${encodeURIComponent(id)}`,
} as const;
