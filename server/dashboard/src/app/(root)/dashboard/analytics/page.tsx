"use client";

import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useApiQuery } from "@/hooks/use-api-query";
import { api } from "@/utils/api";

type DailyCount = { date: string; count: number };
type PathCount = { path: string; count: number };

type AnalyticsSummary = {
  window_days: number;
  operations: number;
  avg_latency_ms: number;
  success_rate: number;
  daily: DailyCount[];
  top_paths: PathCount[];
};

const ANALYTICS_ENDPOINTS = {
  SUMMARY: "/analytics/summary",
} as const;

const DAY_LABEL = new Intl.DateTimeFormat("zh-CN", {
  month: "numeric",
  day: "numeric",
});

const formatDay = (date: string) => {
  const parsed = new Date(`${date}T00:00:00`);
  return Number.isNaN(parsed.getTime()) ? date : DAY_LABEL.format(parsed);
};

export default function AnalyticsPage() {
  const { data, isLoading, error } = useApiQuery<AnalyticsSummary>(
    async () => {
      const res = await api.get<AnalyticsSummary>(ANALYTICS_ENDPOINTS.SUMMARY, {
        params: { days: 12 },
      });
      return res.data;
    },
    { errorToast: "无法加载分析数据" },
  );

  const operations = data?.operations ?? 0;
  const hasTraffic = operations > 0;
  const peak = Math.max(...(data?.daily ?? []).map((point) => point.count), 0);

  const stats = [
    {
      label: "操作总数",
      // A dash is honest: zero requests is "nothing measured", not "zero operations".
      value: hasTraffic ? operations.toLocaleString() : "—",
    },
    {
      label: "平均耗时",
      value: hasTraffic && data ? `${Math.round(data.avg_latency_ms)}ms` : "—",
    },
    {
      label: "成功率",
      value:
        hasTraffic && data ? `${(data.success_rate * 100).toFixed(1)}%` : "—",
    },
  ];

  return (
    <div className="space-y-6">
      <div className="space-y-1">
        <h1 className="text-xl font-semibold font-fustat">分析</h1>
        <p className="text-sm text-onSurface-default-tertiary">
          本机服务处理过的请求
          {data ? `（最近 ${data.window_days} 天）` : ""}
          。数据来自「请求」页展示的 同一份记录，全部保存在这台机器上。
        </p>
      </div>

      {error && (
        <Card className="border-memBorder-primary">
          <CardContent className="p-4 text-sm text-onSurface-default-secondary">
            {error}
          </CardContent>
        </Card>
      )}

      <div className="grid grid-cols-1 md:grid-cols-3 gap-4">
        {stats.map((stat) => (
          <Card key={stat.label} className="border-memBorder-primary">
            <CardContent className="p-4">
              <p className="text-xs text-onSurface-default-tertiary">
                {stat.label}
              </p>
              <p className="text-2xl font-semibold mt-1">
                {isLoading ? "…" : stat.value}
              </p>
            </CardContent>
          </Card>
        ))}
      </div>

      <Card className="border-memBorder-primary">
        <CardHeader className="pb-2">
          <CardTitle className="text-sm">按天统计的操作数</CardTitle>
        </CardHeader>
        <CardContent className="p-4">
          <div className="h-[200px] flex items-end gap-1">
            {(data?.daily ?? []).map((point) => (
              <div
                key={point.date}
                className="flex-1 flex flex-col justify-end h-full"
                title={`${formatDay(point.date)}：${point.count} 次请求`}
              >
                <div
                  className={`w-full rounded-t ${
                    point.count > 0
                      ? "bg-surface-default-brand"
                      : "bg-memBorder-primary"
                  }`}
                  // A day with no traffic keeps a sliver of colour so the axis stays readable.
                  style={{
                    height: peak
                      ? `${Math.max((point.count / peak) * 100, point.count ? 4 : 1)}%`
                      : "1%",
                  }}
                />
              </div>
            ))}
          </div>
          {data && data.daily.length > 0 && (
            <div className="mt-2 flex justify-between text-xs text-onSurface-default-tertiary">
              <span>{formatDay(data.daily[0].date)}</span>
              <span>{formatDay(data.daily[data.daily.length - 1].date)}</span>
            </div>
          )}
        </CardContent>
      </Card>

      <Card className="border-memBorder-primary">
        <CardHeader className="pb-2">
          <CardTitle className="text-sm">调用最多的接口</CardTitle>
        </CardHeader>
        <CardContent className="p-4">
          {!isLoading && (data?.top_paths.length ?? 0) === 0 ? (
            <p className="text-sm text-onSurface-default-tertiary">
              还没有记录到请求。等有 agent
              或脚本调用本服务后，接口就会出现在这里。
            </p>
          ) : (
            <ul className="space-y-2">
              {(data?.top_paths ?? []).map((entry) => (
                <li
                  key={entry.path}
                  className="flex items-center justify-between gap-4 text-sm"
                >
                  <span className="font-mono text-xs text-onSurface-default-secondary truncate">
                    {entry.path}
                  </span>
                  <span className="text-onSurface-default-tertiary tabular-nums">
                    {entry.count.toLocaleString()}
                  </span>
                </li>
              ))}
            </ul>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
