"use client";

import {
  createContext,
  useCallback,
  useEffect,
  useMemo,
  useState,
} from "react";
import { api, setAccessToken } from "@/utils/api";
import { apiUrl } from "@/lib/api-url";
import { AUTH_ENDPOINTS } from "@/utils/api-endpoints";
import {
  clearRefreshToken,
  refreshSession,
  storeRefreshToken,
} from "@/lib/session";

export interface AuthUser {
  id: string;
  name: string;
  email: string;
  role: string;
  created_at: string;
}

interface AuthContextValue {
  user: AuthUser | null;
  isLoading: boolean;
  isAdmin: boolean;
  login: (email: string, password: string) => Promise<void>;
  register: (name: string, email: string, password: string) => Promise<void>;
  logout: () => Promise<void>;
  refreshUser: () => Promise<void>;
}

export const AuthContext = createContext<AuthContextValue>({
  user: null,
  isLoading: true,
  isAdmin: false,
  login: async () => {},
  register: async () => {},
  logout: async () => {},
  refreshUser: async () => {},
});

async function restoreSession(): Promise<boolean> {
  const accessToken = await refreshSession();
  if (!accessToken) return false;
  setAccessToken(accessToken);
  return true;
}

/**
 * Ask the server who we are when there is no session to restore.
 *
 * A local install (AUTH_DISABLED) authenticates nobody, so there is no token to refresh and no
 * login to perform — but the dashboard still needs a user to render, and `/auth/me` answers for the
 * local caller. This uses a bare fetch rather than the shared client so a 401 does not run the
 * interceptor's redirect: on an install that does have accounts, that would loop through /login
 * every time it loaded.
 */
async function loadLocalUser(): Promise<AuthUser | null> {
  try {
    const res = await fetch(apiUrl(AUTH_ENDPOINTS.ME));
    if (!res.ok) return null;
    return (await res.json()) as AuthUser;
  } catch {
    return null;
  }
}

export function AuthProvider({ children }: { children: React.ReactNode }) {
  const [user, setUser] = useState<AuthUser | null>(null);
  const [isLoading, setIsLoading] = useState(true);

  const loadUser = useCallback(async () => {
    const res = await api.get<AuthUser>(AUTH_ENDPOINTS.ME);
    setUser(res.data);
  }, []);

  useEffect(() => {
    let active = true;
    (async () => {
      try {
        const restored = await restoreSession();
        if (!active) return;

        if (restored) {
          await loadUser();
        } else {
          const localUser = await loadLocalUser();
          if (localUser && active) setUser(localUser);
        }
      } catch {
        if (active) setUser(null);
      } finally {
        if (active) setIsLoading(false);
      }
    })();
    return () => {
      active = false;
    };
  }, [loadUser]);

  const login = useCallback(
    async (email: string, password: string) => {
      const res = await api.post(AUTH_ENDPOINTS.LOGIN, { email, password });
      setAccessToken(res.data.access_token);
      await storeRefreshToken(res.data.refresh_token);
      await loadUser();
    },
    [loadUser],
  );

  const register = useCallback(
    async (name: string, email: string, password: string) => {
      const res = await api.post(AUTH_ENDPOINTS.REGISTER, {
        name,
        email,
        password,
      });
      setAccessToken(res.data.access_token);
      await storeRefreshToken(res.data.refresh_token);
      await loadUser();
    },
    [loadUser],
  );

  const logout = useCallback(async () => {
    await clearRefreshToken();
    setAccessToken(null);
    setUser(null);
    if (typeof window !== "undefined") window.location.href = "/login";
  }, []);

  const value = useMemo<AuthContextValue>(
    () => ({
      user,
      isLoading,
      isAdmin: user?.role === "admin",
      login,
      register,
      logout,
      refreshUser: loadUser,
    }),
    [user, isLoading, login, register, logout, loadUser],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}
