import { api } from "@/utils/api";
import { EXPORT_ENDPOINTS } from "@/utils/api-endpoints";

/**
 * One page of an export. Mirrors the server's `/export` response.
 *
 * `total` is how many rows are in this response; `total_in_store` is how many rows the
 * collection holds. They differ exactly when there is more to fetch, which is what `truncated`
 * says outright.
 */
export type ExportPage = {
  exported_at: string;
  total: number;
  total_in_store: number;
  offset: number;
  truncated: boolean;
  memories: unknown[];
};

/** The server's page cap, `EXPORT_PAGE_LIMIT` in routes_misc.rs. Asking for more is clamped. */
export const EXPORT_PAGE_LIMIT = 1000;

/**
 * A guard against looping forever if the server ever contradicts itself.
 *
 * Every page is one round trip; 1000 pages is a million memories and far past anything this
 * app is sized for. Hitting it is a bug, not a slow export.
 */
const MAX_PAGES = 1000;

export class ExportIncomplete extends Error {
  constructor(
    readonly fetched: number,
    readonly total: number,
    readonly reason: string,
  ) {
    super(`导出未完成：已取到 ${fetched} / 共 ${total} 条（${reason}）`);
    this.name = "ExportIncomplete";
  }
}

/**
 * Fetch every page of the collection and return the rows in one list.
 *
 * The export used to be a single capped request, so a library past the cap was exported short
 * with nothing saying so — which made "export, reset, re-import" silently lose data. Walking
 * the pages is what makes that migration path safe, and it is why this returns a hard error
 * instead of a partial list: a caller that downloads a partial file as a backup has been lied
 * to. Callers decide what to do with `total_in_store`, but nobody gets a silent partial.
 */
export const fetchAllMemories = async (): Promise<{
  memories: unknown[];
  totalInStore: number;
}> => {
  const memories: unknown[] = [];
  let offset = 0;
  let totalInStore = 0;

  for (let page = 0; page < MAX_PAGES; page += 1) {
    const res = await api.get(EXPORT_ENDPOINTS.EXPORT, {
      params: { format: "json", offset, limit: EXPORT_PAGE_LIMIT },
    });
    const body = res.data as ExportPage;
    const rows = Array.isArray(body?.memories) ? body.memories : [];
    totalInStore =
      typeof body?.total_in_store === "number"
        ? body.total_in_store
        : memories.length + rows.length;

    memories.push(...rows);

    // Stop on an empty page as well as on the flag: an offset past the end returns nothing,
    // and trusting `truncated` alone there would spin.
    if (rows.length === 0 || body?.truncated !== true) {
      if (memories.length < totalInStore) {
        // The server said there is nothing more but the count disagrees. That is a server bug;
        // refusing to hand back a short list is the whole point of this function.
        throw new ExportIncomplete(
          memories.length,
          totalInStore,
          "服务端返回的页比统计到的最少条数还少",
        );
      }
      return { memories, totalInStore };
    }

    offset += rows.length;
  }

  throw new ExportIncomplete(memories.length, totalInStore, "分页次数超出上限");
};

/** Trigger a browser download of `data` as pretty-printed JSON. */
export const downloadJson = (data: unknown, filename: string) => {
  const blob = new Blob([JSON.stringify(data, null, 2)], {
    type: "application/json",
  });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = filename;
  document.body.appendChild(link);
  link.click();
  link.remove();
  // Revoking immediately can cancel the download in some browsers.
  setTimeout(() => URL.revokeObjectURL(url), 1000);
};

/** `mem0-memories-20260118-1930.json` — a timestamp so two backups never overwrite each other. */
export const backupFilename = (now: Date = new Date()) => {
  const pad = (n: number) => String(n).padStart(2, "0");
  const stamp =
    `${now.getFullYear()}${pad(now.getMonth() + 1)}${pad(now.getDate())}` +
    `-${pad(now.getHours())}${pad(now.getMinutes())}`;
  return `mem0-memories-${stamp}.json`;
};
