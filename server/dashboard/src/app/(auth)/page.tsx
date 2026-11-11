"use client";

import { useEffect } from "react";
import { useRouter } from "next/navigation";
import { apiUrl } from "@/lib/api-url";
import { AUTH_ENDPOINTS } from "@/utils/api-endpoints";

/**
 * Entry route.
 *
 * The Docker build redirects `/` in middleware. A static export has no middleware, so this page
 * does the same decision in the browser: a fresh install goes to onboarding, everyone else to the
 * login screen (which redirects onward to the dashboard once a session exists).
 */
export default function IndexPage() {
  const router = useRouter();

  useEffect(() => {
    let active = true;

    (async () => {
      try {
        const res = await fetch(apiUrl(AUTH_ENDPOINTS.SETUP_STATUS));
        if (res.ok && active) {
          const { needsSetup, localMode } = await res.json();
          if (needsSetup) {
            router.replace("/setup/");
          } else if (localMode) {
            // Nothing to log into: the server answers for the local caller.
            router.replace("/dashboard/requests/");
          } else {
            router.replace("/login/");
          }
          return;
        }
      } catch {
        // API unreachable; the login screen surfaces the connection error.
      }
      if (active) router.replace("/login/");
    })();

    return () => {
      active = false;
    };
  }, [router]);

  return (
    <main className="flex min-h-screen items-center justify-center">
      <p className="text-sm text-muted-foreground">加载中…</p>
    </main>
  );
}
