"use client";

import { useEffect, useState } from "react";

import { api } from "@/utils/api";
import { DIAGNOSTICS_ENDPOINTS } from "@/utils/api-endpoints";

interface StorageDiagnostics {
  data_dir?: string;
  runtime_configured?: boolean;
  config_error?: string | null;
  has_memories?: boolean;
  legacy_python_data?: string[];
}

/**
 * Why the memory list is empty, when it is.
 *
 * "No memories yet" is only one of the reasons an empty list appears: the runtime may not
 * be configured, the data directory may not be the one the operator expects, or it may
 * still hold the Python build's DuckDB store, which this build cannot read. The server
 * knows which, so ask it — a blank page with no explanation sends people looking in the
 * wrong place.
 *
 * The wording lives here rather than in the response: the API keeps reporting the facts
 * (`runtime_configured`, `config_error`, `legacy_python_data`, `data_dir`) so the same
 * payload can serve a log, a script, or this page.
 *
 * Renders nothing when there is nothing to say (memories exist) or while loading.
 */
export function StorageHint({ enabled }: { enabled: boolean }) {
  const [hint, setHint] = useState<string | null>(null);

  useEffect(() => {
    if (!enabled) {
      setHint(null);
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const res = await api.get<StorageDiagnostics>(
          DIAGNOSTICS_ENDPOINTS.STORAGE,
        );
        if (!cancelled) setHint(explain(res.data));
      } catch {
        // A diagnostic that fails must not replace the page's own error state.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [enabled]);

  if (!hint) return null;
  return (
    <p
      data-testid="storage-hint"
      className="text-xs text-onSurface-default-tertiary leading-relaxed"
    >
      {hint}
    </p>
  );
}

/** Same precedence as the server's own hint: config, then data on disk. */
function explain(data: StorageDiagnostics | undefined): string | null {
  if (!data) return null;

  if (!data.runtime_configured) {
    return data.config_error
      ? `无法构建 provider 运行时，记忆读不出来：${data.config_error}。请修正配置后刷新页面。`
      : "还没有配置任何 provider，记忆无法读取。打开 /setup 完成配置。";
  }

  if (data.has_memories) return null;

  const legacy = data.legacy_python_data ?? [];
  if (legacy.length > 0) {
    return `这个数据目录里是 Python 版留下的记忆（${legacy.join("、")}），本应用读不了：它们存在 DuckDB 文件里，而本应用把记忆存在 vectors.db。请先在 Python 版里导出 JSON，再回到这里的「导出」页导入。`;
  }

  return `${
    data.data_dir ?? "数据目录"
  } 里的记忆库是空的。用其他数据目录写入的记忆不会出现在这里；具体用哪个目录由 MEM0_DATA_DIR 决定。`;
}
