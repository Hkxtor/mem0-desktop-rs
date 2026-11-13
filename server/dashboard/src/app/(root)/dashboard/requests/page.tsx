"use client";

import { useState } from "react";
import { format, formatDistanceToNow } from "date-fns";
import { zhCN } from "date-fns/locale";
import { RefreshCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { DataTable } from "@/components/shared/data-table";
import { TableSkeleton } from "@/components/shared/table-skeleton";
import { EmptyState } from "@/components/self-hosted/empty-state";
import { api } from "@/utils/api";
import { REQUEST_ENDPOINTS } from "@/utils/api-endpoints";
import { useApiQuery } from "@/hooks/use-api-query";
import { ApiRequestLog } from "@/types/api";

type RequestLog = {
  id: string;
  createdAt: string;
  operation: string;
  detail: string;
  results: number | null;
  statusCode: number;
  latencyMs: number;
  reason: string | null;
};

/// Why an operation that succeeded produced nothing. The provider's own message lives in
/// the application log, not in this column: a database field is not a place to store prose
/// the UI would have to translate.
const REASON_LABELS: Record<string, string> = {
  no_memories: "未抽出",
  provider_error: "模型调用失败",
  unparsed_answer: "回答无法解析",
};

const REASON_HINTS: Record<string, string> = {
  no_memories: "模型看过这条消息，但没有发现值得记住的内容。",
  provider_error: "模型调用失败，抽取被跳过。详情见「日志」页的应用日志。",
  unparsed_answer:
    "模型的回答不是完整的 JSON——常见原因是回答在输出长度上限处被截断。日志页会写明是截断还是格式错误，以及回答的结尾。",
};

const REQUEST_LOG_LIMIT = 200;
const PAGE_SIZE = 20;

const getStatusClassName = (statusCode: number) => {
  if (statusCode >= 500) {
    return "border-rose-200 bg-rose-50 text-rose-700 dark:border-rose-900/40 dark:bg-rose-950/40 dark:text-rose-300";
  }

  if (statusCode >= 400) {
    return "border-amber-200 bg-amber-50 text-amber-700 dark:border-amber-900/40 dark:bg-amber-950/40 dark:text-amber-300";
  }

  return "border-emerald-200 bg-emerald-50 text-emerald-700 dark:border-emerald-900/40 dark:bg-emerald-950/40 dark:text-emerald-300";
};

const getOperationClassName = (operation: string) => {
  switch (operation.toUpperCase()) {
    case "ADD":
      return "border-emerald-200 bg-emerald-50 text-emerald-700 dark:border-emerald-900/40 dark:bg-emerald-950/40 dark:text-emerald-300";
    case "SEARCH":
      return "border-sky-200 bg-sky-50 text-sky-700 dark:border-sky-900/40 dark:bg-sky-950/40 dark:text-sky-300";
    case "UPDATE":
      return "border-amber-200 bg-amber-50 text-amber-700 dark:border-amber-900/40 dark:bg-amber-950/40 dark:text-amber-300";
    case "DELETE":
    case "DELETE_ALL":
    case "RESET":
      return "border-rose-200 bg-rose-50 text-rose-700 dark:border-rose-900/40 dark:bg-rose-950/40 dark:text-rose-300";
    default:
      return "border-memBorder-primary bg-surface-default-secondary text-onSurface-default-secondary";
  }
};

const normalizeLog = (entry: ApiRequestLog): RequestLog => {
  // This build reports memory operations. A backend that only reports HTTP traffic
  // (method/path) is still rendered rather than showing three empty columns.
  const isMemoryOperation = typeof entry.operation === "string";
  return {
    id: entry.id,
    createdAt: entry.created_at,
    operation: isMemoryOperation
      ? entry.operation!
      : `${entry.method ?? ""} ${entry.path ?? ""}`.trim(),
    detail: isMemoryOperation ? (entry.detail ?? "") : "",
    results: entry.results ?? null,
    statusCode: entry.status_code,
    latencyMs: entry.latency_ms,
    reason: entry.reason ?? null,
  };
};

export default function RequestsPage() {
  const [lastUpdated, setLastUpdated] = useState<string | null>(null);
  const [page, setPage] = useState(0);

  const {
    data: logs = [],
    isLoading,
    error,
    refetch,
  } = useApiQuery<RequestLog[]>(
    async () => {
      const res = await api.get<ApiRequestLog[]>(REQUEST_ENDPOINTS.BASE, {
        params: { limit: REQUEST_LOG_LIMIT },
      });
      setLastUpdated(new Date().toISOString());
      return (res.data ?? []).map(normalizeLog);
    },
    { errorToast: "无法加载操作记录", initialData: [] },
  );

  const columns = [
    {
      key: "createdAt" as keyof RequestLog,
      label: "时间",
      width: 140,
      render: (value: string) => (
        <span title={format(new Date(value), "PPpp", { locale: zhCN })}>
          {formatDistanceToNow(new Date(value), {
            addSuffix: true,
            locale: zhCN,
          })}
        </span>
      ),
    },
    {
      key: "operation" as keyof RequestLog,
      label: "操作",
      width: 130,
      render: (value: string) => (
        <Badge variant="outline" className={getOperationClassName(value)}>
          {value.toUpperCase()}
        </Badge>
      ),
    },
    {
      key: "detail" as keyof RequestLog,
      label: "对象",
      width: 360,
      render: (value: string) => (
        <span className="font-mono text-xs break-all text-onSurface-default-primary">
          {value || "--"}
        </span>
      ),
    },
    {
      key: "results" as keyof RequestLog,
      label: "结果数",
      width: 150,
      render: (value: number | null, row: RequestLog) => {
        const label = row.reason ? REASON_LABELS[row.reason] : undefined;
        return (
          <span className="flex items-center gap-2">
            {value === null ? (
              <span className="text-onSurface-default-tertiary">--</span>
            ) : (
              <span className="tabular-nums">{value}</span>
            )}
            {value === 0 && row.reason && (
              <span
                title={REASON_HINTS[row.reason] ?? row.reason}
                className="cursor-help rounded border border-memBorder-primary bg-surface-default-secondary px-1.5 py-0.5 text-[11px] text-onSurface-default-secondary"
              >
                {label ?? row.reason}
              </span>
            )}
          </span>
        );
      },
    },
    {
      key: "statusCode" as keyof RequestLog,
      label: "状态",
      width: 110,
      render: (value: number) => (
        <Badge variant="outline" className={getStatusClassName(value)}>
          {value}
        </Badge>
      ),
    },
    {
      key: "latencyMs" as keyof RequestLog,
      label: "耗时",
      width: 100,
      render: (value: number) => <span>{Math.round(value)} ms</span>,
    },
  ];

  return (
    <div className="space-y-6">
      <div className="flex items-start justify-between gap-4">
        <div className="space-y-1">
          <h1 className="text-xl font-semibold font-fustat">请求</h1>
          <p className="text-sm text-onSurface-default-secondary">
            对记忆执行过的每一次添加、搜索、更新和删除。
          </p>
          {lastUpdated && (
            <p className="text-xs text-onSurface-default-tertiary">
              最近更新{" "}
              {formatDistanceToNow(new Date(lastUpdated), {
                addSuffix: true,
                locale: zhCN,
              })}
            </p>
          )}
        </div>
        <Button
          variant="outline"
          onClick={() => {
            setPage(0);
            void refetch();
          }}
          disabled={isLoading}
        >
          <RefreshCw className="size-4 mr-2" />
          刷新
        </Button>
      </div>

      {error && (
        <Card className="border-memBorder-primary">
          <CardContent className="p-4 text-sm text-onSurface-danger-primary">
            {error}
          </CardContent>
        </Card>
      )}

      {isLoading ? (
        <TableSkeleton rows={6} columns={5} />
      ) : logs.length === 0 ? (
        <EmptyState
          title="还没有记忆操作"
          description="添加、搜索或删除一条记忆后，这里就会出现记录。"
          image="requests"
        />
      ) : (
        <>
          <Card className="border-memBorder-primary overflow-hidden">
            <DataTable
              data={logs.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE)}
              columns={columns}
              getRowKey={(row) => row.id}
            />
          </Card>
          {logs.length > PAGE_SIZE && (
            <div className="flex items-center justify-between text-sm text-onSurface-default-tertiary">
              <span>
                {page * PAGE_SIZE + 1}–
                {Math.min((page + 1) * PAGE_SIZE, logs.length)} / 共{" "}
                {logs.length} 条
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
                  disabled={(page + 1) * PAGE_SIZE >= logs.length}
                  onClick={() => setPage((p) => p + 1)}
                >
                  下一页
                </Button>
              </div>
            </div>
          )}
        </>
      )}
    </div>
  );
}
