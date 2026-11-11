"use client";

import { useEffect } from "react";
import { usePathname, useRouter } from "next/navigation";
import { useAuth } from "@/hooks/use-auth";

/**
 * Client-side replacement for the auth half of the old `middleware.ts`.
 *
 * A static export has no server to run middleware, so the dashboard renders first and redirects
 * afterwards. Nothing is exposed by that delay: every data request behind this guard still has to
 * pass the backend's own auth check.
 */
function GuardSkeleton() {
  return (
    <div className="flex min-h-screen items-center justify-center">
      <div className="flex flex-col items-center gap-3">
        <div className="h-1 w-32 overflow-hidden rounded-full bg-muted">
          <div className="h-full w-1/2 animate-pulse rounded-full bg-foreground/40" />
        </div>
        <p className="text-xs uppercase tracking-widest text-muted-foreground">
          Connecting
        </p>
      </div>
    </div>
  );
}

export function RouteGuard({ children }: { children: React.ReactNode }) {
  const { user, isLoading } = useAuth();
  const router = useRouter();
  const pathname = usePathname();

  useEffect(() => {
    if (isLoading || user) return;

    const next =
      pathname && pathname !== "/"
        ? `?next=${encodeURIComponent(pathname)}`
        : "";
    router.replace(`/login/${next}`);
  }, [isLoading, user, router, pathname]);

  // While the session is being restored there is nothing honest to render yet.
  if (isLoading) return <GuardSkeleton />;

  // Redirect is in flight; render nothing rather than a flash of empty chrome.
  if (!user) return null;

  return <>{children}</>;
}
