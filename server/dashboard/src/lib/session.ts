import { apiUrl } from "@/lib/api-url";

/**
 * The refresh token lives in an `HttpOnly` cookie owned by the backend. The browser cannot read
 * it, and — in the desktop build — the page and the API share an origin, so `credentials:
 * "include"` is enough for the cookie to travel with each call.
 *
 * These three functions replace what `src/app/api/auth/refresh/route.ts` used to do; a static
 * export cannot contain route handlers.
 */
const SESSION_ENDPOINT = apiUrl("/auth/session");

/** Hand a freshly issued refresh token to the backend so it can be stored as a cookie. */
export async function storeRefreshToken(refreshToken: string): Promise<void> {
  await fetch(SESSION_ENDPOINT, {
    method: "PUT",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ refresh_token: refreshToken }),
  });
}

/** Drop the cookie. Safe to call when there is nothing to drop. */
export async function clearRefreshToken(): Promise<void> {
  await fetch(SESSION_ENDPOINT, { method: "DELETE", credentials: "include" });
}

/**
 * Exchange the refresh cookie for a new access token.
 *
 * Returns `null` when the session is gone (no cookie, expired, already rotated) — the caller
 * decides whether that means "show the login screen" or just "carry on anonymously".
 */
export async function refreshSession(): Promise<string | null> {
  const res = await fetch(SESSION_ENDPOINT, {
    method: "POST",
    credentials: "include",
  });

  if (!res.ok) return null;

  const data = (await res.json()) as { access_token?: string };
  return data.access_token ?? null;
}
