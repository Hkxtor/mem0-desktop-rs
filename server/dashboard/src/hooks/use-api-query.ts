import { useCallback, useEffect, useRef, useState } from "react";
import { toast } from "@/components/ui/use-toast";
import { getErrorMessage } from "@/lib/error-message";

interface UseApiQueryOptions<T> {
  enabled?: boolean;
  errorToast?: string;
  initialData?: T;
  /** Re-fetch on this interval (ms). Polls never toggle `isLoading`, so a table that
   *  refreshes in place does not flash its skeleton on every tick. */
  refetchInterval?: number;
}

interface UseApiQueryResult<T> {
  data: T | undefined;
  isLoading: boolean;
  error: string;
  refetch: () => Promise<void>;
}

export function useApiQuery<T>(
  fetcher: () => Promise<T>,
  options: UseApiQueryOptions<T> = {},
): UseApiQueryResult<T> {
  const { enabled = true, errorToast, initialData, refetchInterval } = options;

  const [data, setData] = useState<T | undefined>(initialData);
  const [isLoading, setIsLoading] = useState(enabled);
  const [error, setError] = useState("");

  const fetcherRef = useRef(fetcher);
  fetcherRef.current = fetcher;

  const run = useCallback(
    async (runOptions: { silent: boolean } | undefined) => {
      const silent = runOptions?.silent === true;
      if (!silent) setIsLoading(true);
      setError("");
      try {
        setData(await fetcherRef.current());
      } catch (err) {
        const message = getErrorMessage(err, errorToast || "Request failed");
        setError(message);
        if (errorToast) {
          toast({
            title: errorToast,
            description: message,
            variant: "destructive",
          });
        }
      } finally {
        if (!silent) setIsLoading(false);
      }
    },
    [errorToast],
  );

  useEffect(() => {
    if (enabled) void run(undefined);
  }, [enabled, run]);

  useEffect(() => {
    if (!enabled || !refetchInterval || refetchInterval <= 0) return;
    const id = setInterval(() => void run({ silent: true }), refetchInterval);
    return () => clearInterval(id);
  }, [enabled, refetchInterval, run]);

  return {
    data,
    isLoading,
    error,
    refetch: useCallback(() => run(undefined), [run]),
  };
}
