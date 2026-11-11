"use client";

import { useMemo, useState } from "react";
import { format, formatDistanceToNow } from "date-fns";
import { zhCN } from "date-fns/locale";
import { Download, RefreshCw, Search } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { DataTable } from "@/components/shared/data-table";
import { TableSkeleton } from "@/components/shared/table-skeleton";
import { EmptyState } from "@/components/self-hosted/empty-state";
import { api } from "@/utils/api";
import { LOG_ENDPOINTS } from "@/utils/api-endpoints";
import { useApiQuery } from "@/hooks/use-api-query";
import { ApiAppLogLine, ApiRequestLog } from "@/types/api";

const APP_LOG_LIMIT = 500;
const REQUEST_LOG_LIMIT = 200;
const PAGE_SIZE = 25;

type LogLevel = "all" | "info" | "warn" | "error";

type AppLogRow = {
  id: string;
  ts: string;
  level: ApiAppLogLine["level"];
  target: string;
  message: string;
  fields: string;
};

type RequestRow = {
  id: string;
  createdAt: string;
  method: string;
  path: string;
  statusCode: number;
  latencyMs: number;
  authType: string;
};

const LEVEL_LABELS: Record<LogLevel, string> = {
  all: "全部级别",
  info: "信息",
  warn: "警告",
  error: "错误",
};

const levelClassName = (level: ApiAppLogLine["level"]) => {
  switch (level) {
    case "error":
      return "border-rose-200 bg-rose-50 text-rose-700 dark:border-rose-900/40 dark:bg-rose-950/40 dark:text-rose-300";
    case "warn":
      return "border-amber-200 bg-amber-50 text-amber-700 dark:border-amber-900/40 dark:bg-amber-950/40 dark:text-amber-300";
    default:
      return "border-sky-200 bg-sky-50 text-sky-700 dark:border-sky-900/40 dark:bg-sky-950/40 dark:text-sky-300";
  }
};

const statusClassName = (statusCode: number) => {
  if (statusCode >= 500) {
    return "border-rose-200 bg-rose-50 text-rose-700 dark:border-rose-900/40 dark:bg-rose-950/40 dark:text-rose-300";
  }
  if (statusCode >= 400) {
    return "border-amber-200 bg-amber-50 text-amber-700 dark:border-amber-900/40 dark:bg-amber-950/40 dark:text-amber-300";
  }
  return "border-emerald-200 bg-emerald-50 text-emerald-700 dark:border-emerald-900/40 dark:bg-emerald-950/40 dark:text-emerald-300";
};

const normalizeAppLine = (line: ApiAppLogLine, index: number): AppLogRow => ({
  id: `${line.ts}-${index}`,
  ts: line.ts,
  level: line.level,
  target: line.target,
  message: line.msg,
  fields:
    line.fields && Object.keys(line.fields).length > 0
      ? JSON.stringify(line.fields)
      : "",
});

const normalizeRequest = (row: ApiRequestLog): RequestRow => ({
  id: row.id,
  createdAt: row.created_at,
  method: row.method ?? "",
  path: row.path ?? "",
  statusCode: row.status_code,
  latencyMs: row.latency_ms,
  authType: row.auth_type ?? "",
});

const AUTH_LABELS: Record<string, string> = {
  bearer: "Bearer",
  api_key: "API 密钥",
  admin_api_key: "管理员密钥",
  disabled: "本地免认证",
  none: "未认证",
};

export default function LogsPage() {
  const [level, setLevel] = useState<LogLevel>("all");
  const [search, setSearch] = useState("");
  const [autoRefresh, setAutoRefresh] = useState(false);
  const [page, setPage] = useState(0);

  const {
    data: appLogData,
    isLoading: appLoading,
    error: appError,
    refetch: refetchApp,
  } = useApiQuery<{
    lines: AppLogRow[];
    initialised: boolean;
    path: string | null;
  }>(
    async () => {
      const res = await api.get<{
        lines: ApiAppLogLine[];
        initialised: boolean;
        path: string | null;
      }>(LOG_ENDPOINTS.APP, { params: { limit: APP_LOG_LIMIT } });
      return {
        lines: (res.data?.lines ?? []).map(normalizeAppLine).reverse(),
        initialised: res.data?.initialised ?? false,
        path: res.data?.path ?? null,
      };
    },
    {
      errorToast: "无法加载应用日志",
      initialData: { lines: [], initialised: false, path: null },
      // The file is appended to constantly; polling it while nobody is looking just
      // fills the request log with reads of itself.
      refetchInterval: autoRefresh ? 5000 : undefined,
    },
  );

  const appLog = appLogData ?? {
    lines: [],
    initialised: false,
    path: null,
  };

  const {
    data: requests = [],
    isLoading: requestLoading,
    error: requestError,
    refetch: refetchRequests,
  } = useApiQuery<RequestRow[]>(
    async () => {
      const res = await api.get<{ requests: ApiRequestLog[] }>(
        LOG_ENDPOINTS.REQUESTS,
        { params: { limit: REQUEST_LOG_LIMIT } },
      );
      return (res.data?.requests ?? []).map(normalizeRequest);
    },
    { errorToast: "无法加载请求日志", initialData: [] },
  );

  const visibleAppLines = useMemo(() => {
    const needle = search.trim().toLowerCase();
    return appLog.lines.filter((line) => {
      if (level !== "all" && line.level !== level) return false;
      if (!needle) return true;
      return (
        line.message.toLowerCase().includes(needle) ||
        line.target.toLowerCase().includes(needle) ||
        line.fields.toLowerCase().includes(needle)
      );
    });
  }, [appLog.lines, level, search]);

  const appColumns = [
    {
      key: "ts" as keyof AppLogRow,
      label: "时间",
      width: 110,
      render: (value: string) => (
        <span
          className="tabular-nums text-xs"
          title={format(new Date(value), "PPpp", { locale: zhCN })}
        >
          {format(new Date(value), "MM-dd HH:mm:ss")}
        </span>
      ),
    },
    {
      key: "level" as keyof AppLogRow,
      label: "级别",
      width: 70,
      render: (value: ApiAppLogLine["level"]) => (
        <Badge variant="outline" className={levelClassName(value)}>
          {LEVEL_LABELS[value]}
        </Badge>
      ),
    },
    {
      key: "target" as keyof AppLogRow,
      label: "来源",
      width: 80,
      render: (value: string) => (
        <span className="font-mono text-xs text-onSurface-default-secondary">
          {value}
        </span>
      ),
    },
    {
      key: "message" as keyof AppLogRow,
      label: "内容",
      width: 520,
      render: (value: string, row: AppLogRow) => (
        // Both lines are single-line and truncated: a log table is scanned, not read,
        // and the full text is one hover away.
        <div className="min-w-0 space-y-0.5">
          <p className="truncate text-onSurface-default-primary" title={value}>
            {value}
          </p>
          {row.fields && (
            <p
              className="truncate font-mono text-[11px] text-onSurface-default-tertiary"
              title={row.fields}
            >
              {row.fields}
            </p>
          )}
        </div>
      ),
    },
  ];

  const requestColumns = [
    {
      key: "createdAt" as keyof RequestRow,
      label: "时间",
      width: 130,
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
      key: "method" as keyof RequestRow,
      label: "方法",
      width: 80,
      render: (value: string) => (
        <span className="font-mono text-xs">{value}</span>
      ),
    },
    {
      key: "path" as keyof RequestRow,
      label: "路径",
      width: 420,
      render: (value: string) => (
        <span className="block truncate font-mono text-xs" title={value}>
          {value}
        </span>
      ),
    },
    {
      key: "statusCode" as keyof RequestRow,
      label: "状态",
      width: 90,
      render: (value: number) => (
        <Badge variant="outline" className={statusClassName(value)}>
          {value}
        </Badge>
      ),
    },
    {
      key: "latencyMs" as keyof RequestRow,
      label: "耗时",
      width: 90,
      render: (value: number) => <span>{Math.round(value)} ms</span>,
    },
    {
      key: "authType" as keyof RequestRow,
      label: "认证",
      width: 110,
      render: (value: string) => (
        <span className="text-xs text-onSurface-default-secondary">
          {AUTH_LABELS[value] ?? value ?? "--"}
        </span>
      ),
    },
  ];

  const downloadAppLog = () => {
    const text = appLog.lines
      .slice()
      .reverse()
      .map((line) =>
        JSON.stringify({
          ts: line.ts,
          level: line.level,
          target: line.target,
          msg: line.message,
          fields: line.fields ? JSON.parse(line.fields) : {},
        }),
      )
      .join("\n");
    const blob = new Blob([`${text}\n`], { type: "application/x-ndjson" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = "app.log";
    a.click();
    URL.revokeObjectURL(url);
  };

  return (
    <div className="space-y-6">
      <div className="flex items-start justify-between gap-4">
        <div className="space-y-1">
          <h1 className="text-xl font-semibold font-fustat">日志</h1>
          <p className="text-sm text-onSurface-default-secondary">
            应用日志记录程序为什么这样运行，请求日志记录到达服务端的每一次调用。
          </p>
          {appLog.path && (
            <p className="font-mono text-xs text-onSurface-default-tertiary">
              {appLog.path}
            </p>
          )}
        </div>
        <div className="flex items-center gap-2">
          <Button
            variant={autoRefresh ? "default" : "outline"}
            onClick={() => setAutoRefresh((v) => !v)}
            title="每 5 秒重新读取一次"
          >
            {autoRefresh ? "自动刷新：开" : "自动刷新：关"}
          </Button>
          <Button
            variant="outline"
            onClick={() => {
              setPage(0);
              void refetchApp();
              void refetchRequests();
            }}
          >
            <RefreshCw className="size-4 mr-2" />
            刷新
          </Button>
        </div>
      </div>

      <Tabs defaultValue="app">
        <TabsList>
          <TabsTrigger value="app">应用日志</TabsTrigger>
          <TabsTrigger value="requests">请求日志</TabsTrigger>
        </TabsList>

        <TabsContent value="app" className="space-y-4 pt-4">
          <div className="flex flex-wrap items-center gap-2">
            <div className="relative flex-1 min-w-[240px]">
              <Search className="absolute left-3 top-1/2 size-4 -translate-y-1/2 text-onSurface-default-tertiary" />
              <Input
                value={search}
                onChange={(e) => {
                  setSearch(e.target.value);
                  setPage(0);
                }}
                placeholder="搜索内容、来源或字段"
                className="pl-9"
                data-testid="log-search"
              />
            </div>
            <Select
              value={level}
              onValueChange={(v) => {
                setLevel(v as LogLevel);
                setPage(0);
              }}
            >
              <SelectTrigger className="w-[140px]" data-testid="log-level">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {(Object.keys(LEVEL_LABELS) as LogLevel[]).map((key) => (
                  <SelectItem key={key} value={key}>
                    {LEVEL_LABELS[key]}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            <Button
              variant="outline"
              onClick={downloadAppLog}
              disabled={appLog.lines.length === 0}
            >
              <Download className="size-4 mr-2" />
              下载
            </Button>
          </div>

          {!appLog.initialised && (
            <Card className="border-memBorder-primary">
              <CardContent className="p-4 text-sm text-onSurface-default-secondary">
                日志尚未初始化。用桌面应用或 `--headless` 启动一次后，这里会写入
                `&lt;data&gt;/logs/app.log`。
              </CardContent>
            </Card>
          )}

          {appError && (
            <Card className="border-memBorder-primary">
              <CardContent className="p-4 text-sm text-onSurface-danger-primary">
                {appError}
              </CardContent>
            </Card>
          )}

          {appLoading ? (
            <TableSkeleton rows={6} columns={4} />
          ) : visibleAppLines.length === 0 ? (
            <EmptyState
              title={
                appLog.lines.length === 0 ? "还没有日志" : "没有匹配的日志"
              }
              description={
                appLog.lines.length === 0
                  ? "启动服务后，这里会记录配置、模型调用、存储和鉴权事件。"
                  : "换个级别或关键字试试。"
              }
              image="requests"
            />
          ) : (
            <>
              <Card className="border-memBorder-primary overflow-hidden">
                <DataTable
                  data={visibleAppLines.slice(
                    page * PAGE_SIZE,
                    (page + 1) * PAGE_SIZE,
                  )}
                  columns={appColumns}
                  getRowKey={(row) => row.id}
                />
              </Card>
              {visibleAppLines.length > PAGE_SIZE && (
                <div className="flex items-center justify-between text-sm text-onSurface-default-tertiary">
                  <span>
                    {page * PAGE_SIZE + 1}–
                    {Math.min((page + 1) * PAGE_SIZE, visibleAppLines.length)} /
                    共 {visibleAppLines.length} 条
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
                      disabled={
                        (page + 1) * PAGE_SIZE >= visibleAppLines.length
                      }
                      onClick={() => setPage((p) => p + 1)}
                    >
                      下一页
                    </Button>
                  </div>
                </div>
              )}
            </>
          )}
        </TabsContent>

        <TabsContent value="requests" className="space-y-4 pt-4">
          {requestError && (
            <Card className="border-memBorder-primary">
              <CardContent className="p-4 text-sm text-onSurface-danger-primary">
                {requestError}
              </CardContent>
            </Card>
          )}

          {requestLoading ? (
            <TableSkeleton rows={6} columns={6} />
          ) : requests.length === 0 ? (
            <EmptyState
              title="还没有请求记录"
              description="任何一次 API 调用都会在这里留下方法、路径、状态码和耗时。"
              image="requests"
            />
          ) : (
            <Card className="border-memBorder-primary overflow-hidden">
              <DataTable
                data={requests}
                columns={requestColumns}
                getRowKey={(row) => row.id}
              />
            </Card>
          )}
        </TabsContent>
      </Tabs>
    </div>
  );
}
