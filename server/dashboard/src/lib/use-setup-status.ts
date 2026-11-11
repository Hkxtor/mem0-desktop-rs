"use client";

import { useEffect, useState } from "react";
import { apiUrl } from "@/lib/api-url";
import { AUTH_ENDPOINTS } from "@/utils/api-endpoints";

export type SetupStatus =
  | { state: "loading" }
  | { state: "needs-setup"; localMode: boolean }
  | { state: "configured"; localMode: boolean }
  | { state: "unreachable" };

/**
 * Ask the backend whether this install still needs the onboarding wizard.
 *
 * Middleware used to answer this before a page rendered. A static export cannot run middleware,
 * so each auth-facing page asks for itself and redirects. `unreachable` is deliberately distinct
 * from `configured`: a backend that has not finished starting must not send a configured install
 * back through onboarding.
 *
 * `localMode` says whether there are accounts at all. A local install authenticates nobody, so its
 * wizard starts at provider configuration and never asks for a login.
 */
export function useSetupStatus(): SetupStatus {
  const [status, setStatus] = useState<SetupStatus>({ state: "loading" });

  useEffect(() => {
    let active = true;

    (async () => {
      try {
        const res = await fetch(apiUrl(AUTH_ENDPOINTS.SETUP_STATUS));
        if (!res.ok) throw new Error(`setup-status returned ${res.status}`);
        const body = (await res.json()) as {
          needsSetup?: boolean;
          localMode?: boolean;
        };
        if (active) {
          setStatus({
            state: body.needsSetup ? "needs-setup" : "configured",
            localMode: !!body.localMode,
          });
        }
      } catch {
        if (active) setStatus({ state: "unreachable" });
      }
    })();

    return () => {
      active = false;
    };
  }, []);

  return status;
}
