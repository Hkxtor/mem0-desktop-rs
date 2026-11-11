"use client";

import { useEffect, useRef, useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";
import Link from "next/link";
import { ExternalLink, RefreshCw, Tags, Trash2, X } from "lucide-react";
import { format } from "date-fns";
import { Button } from "@/components/ui/button";
import { displayApiUrl } from "@/lib/api-url";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { DataTable } from "@/components/shared/data-table";
import { TableSkeleton } from "@/components/shared/table-skeleton";
import { EmptyState } from "@/components/self-hosted/empty-state";
import { StorageHint } from "@/components/self-hosted/storage-hint";
import DeleteConfirmationModal from "@/components/ui/delete-confirmation-modal";
import {
  Sheet,
  SheetContent,
  SheetHeader,
  SheetTitle,
  SheetDescription,
} from "@/components/ui/sheet";
import { Tabs, TabsList, TabsTrigger, TabsContent } from "@/components/ui/tabs";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { toast } from "@/components/ui/use-toast";
import { getErrorMessage } from "@/lib/error-message";
import { api } from "@/utils/api";
import { MEMORY_ENDPOINTS, SYSTEMONE_ENDPOINTS } from "@/utils/api-endpoints";
import { useApiQuery } from "@/hooks/use-api-query";
import { Memory, MemoryHistoryEntry, MemoryInput } from "@/types/api";
import { CategoriesDisplay } from "@/components/ui/categories-display";

const PAGE_SIZE = 20;
// Keep in sync with ALL_MEMORIES_LIMIT in server/main.py.
const MEMORY_FETCH_LIMIT = 1000;
// How many memories one classification request may handle. One provider call each
// (2-5s), so 20 keeps a single request near a minute.
const CLASSIFY_BATCH_SIZE = 20;

const TIME_RANGES = [
  { value: "all", label: "全部时间", days: null },
  { value: "1d", label: "最近 24 小时", days: 1 },
  { value: "7d", label: "最近 7 天", days: 7 },
  { value: "30d", label: "最近 30 天", days: 30 },
] as const;

interface CategoryStats {
  configured: boolean;
  unclassified: number;
  last_error?: string | null;
}

/**
 * Compact relative time ("5h ago", "2d ago").
 *
 * `formatDistanceToNow` spells it out ("less than a minute ago"), which wraps to two
 * lines in a 120px column and reads as noise in a list.
 */
function relativeTime(value: string): string {
  const then = new Date(value).getTime();
  if (Number.isNaN(then)) return "--";
  const seconds = Math.max(0, Math.round((Date.now() - then) / 1000));
  if (seconds < 60) return "刚刚";
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  const days = Math.round(hours / 24);
  if (days < 30) return `${days} 天前`;
  const months = Math.round(days / 30);
  if (months < 12) return `${months} 个月前`;
  return `${Math.round(months / 12)} 年前`;
}

/** An expired memory is hidden from lists and search, so say so rather than "Active". */
function lifecycleOf(memory: Memory): "Active" | "Expired" {
  const raw = memory.expiration_date;
  if (!raw) return "Active";
  const today = new Date().toISOString().slice(0, 10);
  return raw < today ? "Expired" : "Active";
}

/** The entities a memory is attached to: user, agent, run — in that order. */
function entitiesOf(memory: Memory): string[] {
  return [memory.user_id, memory.agent_id, memory.run_id].filter(
    (value): value is string => Boolean(value && value.trim()),
  );
}

export default function MemoriesPage() {
  const searchParams = useSearchParams();
  const router = useRouter();
  // Category filter from the Categories page: ?categories=food,travel (AND semantics).
  const categoriesParam = searchParams.get("categories") ?? "";
  const activeCategories = categoriesParam
    .split(",")
    .map((c) => c.trim())
    .filter(Boolean);

  const [query, setQuery] = useState("");
  const [range, setRange] = useState<string>("all");
  const [selectedMemory, setSelectedMemory] = useState<Memory | null>(null);
  const [memoryToDelete, setMemoryToDelete] = useState<Memory | null>(null);
  const [page, setPage] = useState(0);
  const apiBase = displayApiUrl();

  // Source inputs and change history for the open memory.
  const [inputs, setInputs] = useState<MemoryInput[]>([]);
  const [history, setHistory] = useState<MemoryHistoryEntry[]>([]);
  const [detailLoading, setDetailLoading] = useState(false);

  // Batch classification: runs in the page, one request per chunk, so a long run is
  // visible and stoppable instead of one request that never returns.
  const [classifying, setClassifying] = useState(false);
  const [progress, setProgress] = useState({ done: 0, total: 0 });
  const [reclassifying, setReclassifying] = useState(false);
  // Ids the model answered "nothing fits" for, this session. Skipped on the next run
  // so an empty answer is not paid for twice.
  const noMatchRef = useRef<string[]>([]);
  const cancelRef = useRef(false);

  const {
    data: memories = [],
    isLoading,
    error,
    refetch,
  } = useApiQuery<Memory[]>(
    async () => {
      const params: Record<string, string | number> = {
        top_k: MEMORY_FETCH_LIMIT,
      };
      if (activeCategories.length > 0)
        params.categories = activeCategories.join(",");
      const res = await api.get(MEMORY_ENDPOINTS.BASE, { params });
      const raw = res.data?.results ?? res.data ?? [];
      return Array.isArray(raw) ? raw : [];
    },
    { errorToast: "无法加载记忆", initialData: [] },
  );

  const { data: stats, refetch: refetchStats } = useApiQuery<CategoryStats>(
    async () => {
      const res = await api.get(SYSTEMONE_ENDPOINTS.COUNTS);
      return {
        configured: Boolean(res.data?.configured),
        unclassified: Number(res.data?.unclassified ?? 0),
        last_error: res.data?.last_error ?? null,
      };
    },
    { initialData: { configured: false, unclassified: 0 } },
  );
  const unclassified = stats?.unclassified ?? 0;
  const classifierConfigured = stats?.configured ?? false;

  // Re-fetch when the URL category filter changes (the query hook only refetches
  // on demand, so drive it from here).
  useEffect(() => {
    setPage(0);
    void refetch();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [categoriesParam]);

  // Load the open memory's source inputs and changelog together.
  useEffect(() => {
    const id = selectedMemory?.id;
    if (!id) {
      setInputs([]);
      setHistory([]);
      return;
    }
    let cancelled = false;
    setDetailLoading(true);
    setInputs([]);
    setHistory([]);
    void (async () => {
      try {
        const [inputRes, historyRes] = await Promise.all([
          api.get(MEMORY_ENDPOINTS.INPUTS(id)),
          api.get(MEMORY_ENDPOINTS.HISTORY(id)),
        ]);
        if (cancelled) return;
        const inRows = inputRes.data;
        setInputs(Array.isArray(inRows) ? inRows : []);
        const histRows = historyRes.data;
        setHistory(Array.isArray(histRows) ? histRows : []);
      } catch (err) {
        if (!cancelled)
          toast({
            title: "无法加载记忆详情",
            description: getErrorMessage(err),
            variant: "destructive",
          });
      } finally {
        if (!cancelled) setDetailLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [selectedMemory?.id]);

  const rangeDays = TIME_RANGES.find((r) => r.value === range)?.days ?? null;
  const visibleMemories = memories
    .filter((m) => {
      if (rangeDays === null) return true;
      const created = m.created_at ? new Date(m.created_at).getTime() : 0;
      return Date.now() - created <= rangeDays * 24 * 60 * 60 * 1000;
    })
    .filter((m) => {
      const needle = query.trim().toLowerCase();
      if (!needle) return true;
      return (
        m.memory.toLowerCase().includes(needle) ||
        entitiesOf(m).some((e) => e.toLowerCase().includes(needle))
      );
    })
    // Newest first. The store returns insertion order, which is not creation order once
    // an import brings in memories with their original timestamps.
    .sort(
      (a, b) =>
        (b.created_at ? new Date(b.created_at).getTime() : 0) -
        (a.created_at ? new Date(a.created_at).getTime() : 0),
    );

  const totalPages = Math.ceil(visibleMemories.length / PAGE_SIZE);
  const paginatedMemories = visibleMemories.slice(
    page * PAGE_SIZE,
    (page + 1) * PAGE_SIZE,
  );

  const handleDelete = async () => {
    if (!memoryToDelete) return;
    try {
      await api.delete(MEMORY_ENDPOINTS.BY_ID(memoryToDelete.id));
      toast({ title: "记忆已删除", variant: "success" });
      if (selectedMemory?.id === memoryToDelete.id) setSelectedMemory(null);
      setMemoryToDelete(null);
      void refetch();
      void refetchStats();
    } catch (error) {
      toast({
        title: "删除记忆失败",
        description: getErrorMessage(error),
        variant: "destructive",
      });
    }
  };

  /**
   * Classify every memory that carries no category, in chunks.
   *
   * Deliberately not scoped to the table's filters: this is maintenance, and the goal is
   * "nothing in the store is untagged", not "the page I happen to be looking at".
   */
  const handleClassifyAll = async () => {
    if (classifying) return;
    cancelRef.current = false;
    setClassifying(true);
    const total = unclassified;
    let done = 0;
    let labeled = 0;
    let noMatch = 0;
    let failed = 0;
    let stopped: string | null = null;
    setProgress({ done: 0, total });
    const skip = [...noMatchRef.current];
    try {
      for (;;) {
        if (cancelRef.current) break;
        const res = await api.post(SYSTEMONE_ENDPOINTS.CLASSIFY_BATCH, {
          limit: CLASSIFY_BATCH_SIZE,
          skip,
        });
        const chunk = res.data ?? {};
        const processed = Number(chunk.processed ?? 0);
        done += processed;
        labeled += Number(chunk.labeled ?? 0);
        noMatch += Number(chunk.no_match ?? 0);
        failed += Number(chunk.failed ?? 0);
        if (Array.isArray(chunk.no_match_ids)) skip.push(...chunk.no_match_ids);
        setProgress({ done, total });
        stopped = chunk.stopped_reason ?? null;
        if (stopped) break;
        if (Number(chunk.remaining ?? 0) <= 0) break;
        // Nothing was attempted and nothing is left to attempt: stop rather than spin.
        if (processed === 0) break;
      }
    } catch (err) {
      stopped = getErrorMessage(err);
    } finally {
      noMatchRef.current = skip;
      setClassifying(false);
      const parts = [`已打标签 ${labeled} 条`];
      if (noMatch > 0) parts.push(`${noMatch} 条没有匹配的分类`);
      if (failed > 0) parts.push(`${failed} 条失败`);
      toast({
        title: stopped ? "分类已中止" : "分类完成",
        description: stopped
          ? `${parts.join(" · ")}. ${stopped}`
          : parts.join(" · "),
        variant: stopped ? "destructive" : "success",
      });
      void refetch();
      void refetchStats();
    }
  };

  const handleReclassify = async (memory: Memory) => {
    setReclassifying(true);
    try {
      const res = await api.post(SYSTEMONE_ENDPOINTS.CLASSIFY_MEMORY, {
        memory_id: memory.id,
      });
      const categories: string[] = Array.isArray(res.data?.categories)
        ? res.data.categories
        : [];
      setSelectedMemory((current) =>
        current && current.id === memory.id
          ? { ...current, categories }
          : current,
      );
      toast({
        title: categories.length
          ? `已打标签：${categories.join("、")}`
          : "没有匹配的分类",
        variant: "success",
      });
      void refetch();
      void refetchStats();
    } catch (err) {
      toast({
        title: "重新分类失败",
        description: getErrorMessage(err),
        variant: "destructive",
      });
    } finally {
      setReclassifying(false);
    }
  };

  const columns = [
    {
      key: "created_at" as keyof Memory,
      label: "时间",
      width: 120,
      render: (value: string) =>
        value ? (
          <span title={format(new Date(value), "PPpp")}>
            {relativeTime(value)}
          </span>
        ) : (
          "--"
        ),
    },
    {
      key: "user_id" as keyof Memory,
      label: "实体",
      width: 160,
      render: (_value: string, row: Memory) => {
        const entities = entitiesOf(row);
        if (entities.length === 0)
          return <span className="text-onSurface-default-tertiary">--</span>;
        return (
          <span className="flex items-center gap-1.5">
            <span className="truncate max-w-[110px]">{entities[0]}</span>
            {entities.length > 1 && (
              <Badge
                variant="outline"
                className="px-1 py-0 text-xs"
                title={entities.slice(1).join(", ")}
              >
                +{entities.length - 1}
              </Badge>
            )}
          </span>
        );
      },
    },
    {
      key: "memory" as keyof Memory,
      label: "记忆内容",
      width: 400,
      render: (value: string) => (
        <span className="line-clamp-2 text-sm">{value}</span>
      ),
    },
    {
      key: "expiration_date" as keyof Memory,
      label: "生命周期",
      width: 100,
      render: (_value: string | null | undefined, row: Memory) => {
        const state = lifecycleOf(row);
        return (
          <Badge
            variant="outline"
            className={
              state === "Expired"
                ? "border-memBorder-primary text-onSurface-danger-primary"
                : "border-memBorder-primary text-onSurface-default-secondary"
            }
          >
            {state === "Expired" ? "已过期" : "有效"}
          </Badge>
        );
      },
    },
    {
      key: "id" as keyof Memory,
      label: "操作",
      width: 130,
      render: (_value: string, row: Memory) => (
        <button
          type="button"
          data-testid="expand-memory"
          className="text-xs text-onSurface-default-secondary underline underline-offset-4 hover:text-onSurface-default-primary"
          onClick={(event) => {
            event.stopPropagation();
            setSelectedMemory(row);
          }}
        >
          展开记忆
        </button>
      ),
    },
  ];

  return (
    <div className="space-y-4">
      <h1 className="text-xl font-semibold font-fustat">记忆</h1>

      <div className="flex flex-wrap gap-3 items-center">
        <Input
          data-testid="memories-search"
          placeholder="搜索记忆或实体"
          value={query}
          onChange={(e) => {
            setQuery(e.target.value);
            setPage(0);
          }}
          className="w-64"
        />
        <Select
          value={range}
          onValueChange={(value) => {
            setRange(value);
            setPage(0);
          }}
        >
          <SelectTrigger data-testid="memories-time-range" className="w-40">
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            {TIME_RANGES.map((option) => (
              <SelectItem key={option.value} value={option.value}>
                {option.label}
              </SelectItem>
            ))}
          </SelectContent>
        </Select>

        <Button
          variant="outline"
          size="sm"
          data-testid="memories-refresh"
          onClick={() => {
            void refetch();
            void refetchStats();
          }}
        >
          <RefreshCw className="size-3.5 mr-1" />
          刷新
        </Button>

        {classifying ? (
          <div className="flex items-center gap-2">
            <Button size="sm" disabled data-testid="classify-progress">
              正在分类 {progress.done}
              {progress.total > 0 ? ` / ${progress.total}` : ""}…
            </Button>
            <Button
              size="sm"
              variant="ghost"
              data-testid="classify-cancel"
              onClick={() => {
                // Stops after the chunk in flight: that request is already paid for.
                cancelRef.current = true;
              }}
            >
              取消
            </Button>
          </div>
        ) : (
          <div className="flex items-center gap-2">
            <Button
              size="sm"
              data-testid="classify-unclassified"
              disabled={!classifierConfigured || unclassified === 0}
              onClick={() => void handleClassifyAll()}
            >
              <Tags className="size-3.5 mr-1" />
              分类 {unclassified} 条未分类记忆
            </Button>
            {!classifierConfigured && (
              <span className="text-xs text-onSurface-default-tertiary">
                还没有 SystemOne 模型 ——{" "}
                <Link
                  href="/dashboard/configuration"
                  className="underline underline-offset-4"
                >
                  去配置
                </Link>
              </span>
            )}
          </div>
        )}

        {activeCategories.length > 0 && (
          <div
            data-testid="active-category-filter"
            className="flex items-center gap-1.5 bg-surface-default-fg-secondary border border-memBorder-primary rounded-md px-2 py-1.5 text-xs"
          >
            <span className="text-onSurface-default-secondary">
              分类：{activeCategories.join(" + ")}
            </span>
            <button
              aria-label="清除分类过滤"
              onClick={() => router.push("/dashboard/memories")}
            >
              <X className="size-3.5" />
            </button>
          </div>
        )}
      </div>

      {isLoading ? (
        <TableSkeleton rows={5} columns={5} />
      ) : error ? (
        // A failed request is not an empty store: saying "No memories yet" here would
        // hide a 503 (no provider configured) or a 403 behind an empty state.
        <Card className="border-memBorder-primary">
          <CardContent className="space-y-3 p-6">
            <p className="text-sm font-medium">无法加载记忆</p>
            <p
              data-testid="memories-error"
              className="text-xs text-onSurface-default-tertiary"
            >
              {error}
            </p>
            <StorageHint enabled />
            <Button variant="outline" size="sm" onClick={() => void refetch()}>
              重试
            </Button>
          </CardContent>
        </Card>
      ) : memories.length === 0 ? (
        <EmptyState
          title="还没有记忆"
          description="发一个 POST /memories 请求即可写入第一条记忆。"
        >
          <pre className="text-xs text-left bg-surface-default-secondary p-3 rounded font-mono overflow-x-auto mt-3 max-w-lg">
            {`curl -X POST ${apiBase}/memories \\
  -H "X-API-Key: <your-key>" \\
  -H "Content-Type: application/json" \\
  -d '{"messages": [{"role": "user", "content": "我喜欢徒步"}], "user_id": "alice"}'`}
          </pre>
          <a
            href="https://docs.mem0.ai/open-source/features/rest-api#memory-operations"
            target="_blank"
            rel="noopener noreferrer"
            className="text-xs text-onSurface-default-tertiary underline underline-offset-4 hover:text-onSurface-default-primary mt-2"
          >
            REST API 参考
          </a>
          {/* Why it is empty, when "nothing was added" is not the answer. */}
          <div className="mt-4 max-w-lg">
            <StorageHint enabled />
          </div>
        </EmptyState>
      ) : visibleMemories.length === 0 ? (
        <Card className="border-memBorderPrimary">
          <CardContent className="p-6 text-sm text-onSurface-default-secondary">
            没有符合当前搜索或时间范围的记忆。
          </CardContent>
        </Card>
      ) : (
        <>
          <Card className="border-memBorder-primary overflow-hidden">
            <DataTable
              data={paginatedMemories}
              columns={columns}
              getRowKey={(row) => row.id}
              onRowClick={(row) => setSelectedMemory(row)}
              getRowClassName={(row) =>
                selectedMemory?.id === row.id
                  ? "bg-surface-default-tertiary"
                  : undefined
              }
            />
          </Card>
          {totalPages > 1 && (
            <div className="flex items-center justify-between text-sm text-onSurface-default-tertiary">
              <span>
                {page * PAGE_SIZE + 1}–
                {Math.min((page + 1) * PAGE_SIZE, visibleMemories.length)} / 共{" "}
                {visibleMemories.length} 条
              </span>
              <div className="flex gap-2">
                <Button
                  variant="outline"
                  size="sm"
                  disabled={page === 0}
                  onClick={() => setPage((p) => p - 1)}
                >
                  上一页
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  disabled={page >= totalPages - 1}
                  onClick={() => setPage((p) => p + 1)}
                >
                  下一页
                </Button>
              </div>
            </div>
          )}
        </>
      )}

      <Sheet
        open={!!selectedMemory}
        onOpenChange={(open) => {
          if (!open) setSelectedMemory(null);
        }}
      >
        <SheetContent className="sm:max-w-lg overflow-y-auto">
          <SheetHeader>
            <SheetTitle>记忆详情</SheetTitle>
            <SheetDescription className="sr-only">
              查看记忆内容、分类、来源输入与变更记录
            </SheetDescription>
          </SheetHeader>
          {selectedMemory && (
            <div className="mt-6 space-y-5">
              <div className="space-y-1">
                <Label className="text-xs uppercase tracking-wide text-onSurface-default-tertiary">
                  记忆
                </Label>
                <p
                  data-testid="detail-memory"
                  className="text-sm whitespace-pre-wrap"
                >
                  {selectedMemory.memory}
                </p>
              </div>

              <div className="space-y-1">
                <div className="flex items-center justify-between">
                  <Label className="text-xs uppercase tracking-wide text-onSurface-default-tertiary">
                    分类
                  </Label>
                  <Button
                    variant="ghost"
                    size="sm"
                    data-testid="detail-reclassify"
                    disabled={reclassifying || !classifierConfigured}
                    onClick={() => void handleReclassify(selectedMemory)}
                  >
                    {reclassifying ? "正在分类…" : "重新分类"}
                  </Button>
                </div>
                {(selectedMemory.categories ?? []).length > 0 ? (
                  <CategoriesDisplay
                    categories={selectedMemory.categories ?? []}
                  />
                ) : (
                  <p
                    data-testid="detail-categories-empty"
                    className="text-sm text-onSurface-default-tertiary"
                  >
                    暂无分类。{" "}
                    {classifierConfigured
                      ? "点「重新分类」为这条记忆打标签。"
                      : "先配置一个 SystemOne 模型才能打标签。"}
                  </p>
                )}
              </div>

              <Tabs defaultValue="details">
                <TabsList>
                  <TabsTrigger value="details">详情</TabsTrigger>
                  <TabsTrigger value="input">
                    输入{inputs.length > 0 ? ` (${inputs.length})` : ""}
                  </TabsTrigger>
                  <TabsTrigger value="changelog">
                    变更记录{history.length > 0 ? ` (${history.length})` : ""}
                  </TabsTrigger>
                </TabsList>

                <TabsContent value="details" className="space-y-3 pt-3">
                  <div className="grid grid-cols-2 gap-3">
                    <div className="space-y-1">
                      <Label className="text-xs text-onSurface-default-tertiary">
                        ID
                      </Label>
                      <p className="text-xs font-mono break-all">
                        {selectedMemory.id}
                      </p>
                    </div>
                    {selectedMemory.agent_id && (
                      <div className="space-y-1">
                        <Label className="text-xs text-onSurface-default-tertiary">
                          智能体
                        </Label>
                        <Link
                          href="/dashboard/entities"
                          className="text-sm inline-flex items-center gap-1 underline underline-offset-4"
                        >
                          {selectedMemory.agent_id}
                          <ExternalLink className="size-3" />
                        </Link>
                      </div>
                    )}
                    {selectedMemory.created_at && (
                      <div className="space-y-1">
                        <Label className="text-xs text-onSurface-default-tertiary">
                          创建时间
                        </Label>
                        <p className="text-sm">
                          {format(new Date(selectedMemory.created_at), "PPpp")}
                        </p>
                      </div>
                    )}
                    {selectedMemory.updated_at && (
                      <div className="space-y-1">
                        <Label className="text-xs text-onSurface-default-tertiary">
                          更新时间
                        </Label>
                        <p className="text-sm">
                          {format(new Date(selectedMemory.updated_at), "PPpp")}
                        </p>
                      </div>
                    )}
                    {selectedMemory.expiration_date && (
                      <div className="space-y-1">
                        <Label className="text-xs text-onSurface-default-tertiary">
                          过期时间
                        </Label>
                        <p className="text-sm">
                          {selectedMemory.expiration_date}
                        </p>
                      </div>
                    )}
                  </div>

                  <div className="space-y-1">
                    <Label className="text-xs text-onSurface-default-tertiary">
                      元数据
                    </Label>
                    {selectedMemory.metadata &&
                    Object.keys(selectedMemory.metadata).length > 0 ? (
                      <pre
                        data-testid="detail-metadata"
                        className="text-xs bg-surface-default-secondary p-2 rounded font-mono overflow-x-auto whitespace-pre-wrap"
                      >
                        {JSON.stringify(selectedMemory.metadata, null, 2)}
                      </pre>
                    ) : (
                      <p className="text-sm text-onSurface-default-tertiary">
                        无元数据
                      </p>
                    )}
                  </div>

                  <Button
                    variant="outline"
                    size="sm"
                    className="text-onSurface-danger-primary"
                    onClick={() => setMemoryToDelete(selectedMemory)}
                  >
                    <Trash2 className="size-3.5 mr-1" />
                    删除记忆
                  </Button>
                </TabsContent>

                <TabsContent value="input" className="pt-3">
                  {detailLoading ? (
                    <p className="text-sm text-onSurface-default-tertiary">
                      加载中…
                    </p>
                  ) : inputs.length === 0 ? (
                    <p
                      data-testid="detail-inputs-empty"
                      className="text-sm text-onSurface-default-tertiary"
                    >
                      没有来源记录。在开始记录来源之前写入的记忆（包括导入的）没有这部分证据。
                    </p>
                  ) : (
                    <ol data-testid="detail-inputs" className="space-y-2">
                      {inputs.map((row) => (
                        <li
                          key={row.seq}
                          className="rounded border border-memBorder-primary p-2"
                        >
                          <div className="flex items-center gap-2 text-xs text-onSurface-default-tertiary">
                            <span className="uppercase">
                              {row.role ?? "input"}
                            </span>
                            {row.created_at && (
                              <span>
                                {format(new Date(row.created_at), "PPpp")}
                              </span>
                            )}
                          </div>
                          <p className="text-sm whitespace-pre-wrap mt-1">
                            {row.content}
                          </p>
                        </li>
                      ))}
                    </ol>
                  )}
                </TabsContent>

                <TabsContent value="changelog" className="pt-3">
                  {detailLoading ? (
                    <p className="text-sm text-onSurface-default-tertiary">
                      加载中…
                    </p>
                  ) : history.length === 0 ? (
                    <p className="text-sm text-onSurface-default-tertiary">
                      没有变更记录。
                    </p>
                  ) : (
                    <ol data-testid="detail-changelog" className="space-y-2">
                      {history.map((row) => (
                        <li
                          key={row.id}
                          className="rounded border border-memBorder-primary p-2"
                        >
                          <div className="flex items-center gap-2 text-xs text-onSurface-default-tertiary">
                            <Badge variant="outline" className="px-1 py-0">
                              {row.event}
                            </Badge>
                            {row.created_at && (
                              <span>
                                {format(new Date(row.created_at), "PPpp")}
                              </span>
                            )}
                          </div>
                          {row.old_memory &&
                            row.old_memory !== row.new_memory && (
                              <p className="text-xs mt-1 line-through text-onSurface-default-tertiary">
                                {row.old_memory}
                              </p>
                            )}
                          {row.new_memory && (
                            <p className="text-sm mt-1 whitespace-pre-wrap">
                              {row.new_memory}
                            </p>
                          )}
                        </li>
                      ))}
                    </ol>
                  )}
                </TabsContent>
              </Tabs>
            </div>
          )}
        </SheetContent>
      </Sheet>

      <DeleteConfirmationModal
        isOpen={!!memoryToDelete}
        onClose={() => setMemoryToDelete(null)}
        onConfirm={handleDelete}
        title="删除记忆"
        description="这条记忆会被永久删除，无法撤销。"
        itemName={memoryToDelete?.id ?? ""}
        confirmButtonText="删除"
      />
    </div>
  );
}
