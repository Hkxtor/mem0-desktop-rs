/**
 * Backend base URL for every browser-side request.
 *
 * The Docker image bakes an absolute URL in at build time (`NEXT_PUBLIC_API_URL=http://localhost:8888`).
 * The desktop build leaves it empty so requests resolve against the page's own origin — the Python
 * backend serves both the static dashboard and the API, which makes the port irrelevant and keeps
 * cookies first-party.
 *
 * `||` rather than `??`: an empty string is a legitimate value here and must not fall through.
 */
export const API_URL = process.env.NEXT_PUBLIC_API_URL || "";

/** Join `API_URL` with an app-relative path, tolerating a trailing slash on either side. */
export function apiUrl(path: string): string {
  const base = API_URL.replace(/\/+$/, "");
  const suffix = path.startsWith("/") ? path : `/${path}`;
  return `${base}${suffix}`;
}

/**
 * Absolute base URL for text the user copies out of the UI (curl examples, API docs snippets).
 *
 * When `API_URL` is empty the app is served from the same origin as the API, so the live origin is
 * the correct value — and it is only knowable at runtime, which is exactly why the desktop build
 * cannot use a build-time constant here.
 */
export function displayApiUrl(): string {
  if (API_URL) return API_URL.replace(/\/+$/, "");
  if (typeof window !== "undefined") return window.location.origin;
  return "";
}
