"use client";

import { useRouter } from "next/navigation";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { TableSkeleton } from "@/components/shared/table-skeleton";
import { api } from "@/utils/api";
import { SYSTEMONE_ENDPOINTS } from "@/utils/api-endpoints";
import { useApiQuery } from "@/hooks/use-api-query";

const CATEGORY_LABELS: Record<string, string> = {
  personal_details: "个人信息",
  family: "家庭",
  professional_details: "职业信息",
  sports: "运动",
  travel: "旅行",
  food: "美食",
  music: "音乐",
  health: "健康",
  technology: "技术",
  hobbies: "爱好",
  fashion: "时尚",
  entertainment: "娱乐",
  milestones: "里程碑",
  user_preferences: "用户偏好",
  misc: "其他",
};

type CategoriesResponse = { configured: boolean; categories: string[] };
type CountsResponse = {
  configured: boolean;
  counts: Record<string, number>;
  provider?: string | null;
  model?: string | null;
  // Why the last classification failed. Classification is best-effort, so without this
  // an unreachable provider looks identical to "no category applies".
  last_error?: string | null;
};

export default function CategoriesPage() {
  const router = useRouter();

  const { data: preset, isLoading: loadingPreset } =
    useApiQuery<CategoriesResponse>(
      async () => (await api.get(SYSTEMONE_ENDPOINTS.CATEGORIES)).data,
      {
        errorToast: "无法加载分类",
        initialData: { configured: false, categories: [] },
      },
    );

  const { data: countsData, isLoading: loadingCounts } =
    useApiQuery<CountsResponse>(
      async () => (await api.get(SYSTEMONE_ENDPOINTS.COUNTS)).data,
      {
        errorToast: "无法加载分类计数",
        initialData: { configured: false, counts: {} },
      },
    );

  const categories = preset?.categories?.length
    ? preset.categories
    : Object.keys(CATEGORY_LABELS);
  const counts = countsData?.counts ?? {};
  const configured = preset?.configured ?? false;
  const loading = loadingPreset || loadingCounts;

  return (
    <div className="space-y-4">
      <div className="flex items-start justify-between">
        <div>
          <h1 className="text-xl font-semibold font-fustat">分类</h1>
          <p className="text-sm text-onSurface-default-tertiary mt-1">
            按主题归类的记忆。点击任一分类即可跳转到已过滤的记忆页。
          </p>
        </div>
      </div>

      {countsData?.last_error && (
        <Card className="border-red-500/40">
          <CardContent className="p-4 space-y-2">
            <p className="text-sm font-medium text-red-500">分类正在失败</p>
            <p
              data-testid="categories-last-error"
              className="text-xs font-mono whitespace-pre-wrap break-words text-red-500"
            >
              {countsData.last_error}
            </p>
            <p className="text-xs text-onSurface-default-tertiary">
              新记忆仍会正常保存，只是没有分类。请到「配置」页检查 SystemOne 的
              provider、模型和 Base URL，并点一下那里的「测试」按钮。
            </p>
            <Button
              variant="outline"
              size="sm"
              onClick={() => router.push("/dashboard/configuration")}
            >
              打开配置
            </Button>
          </CardContent>
        </Card>
      )}

      {!configured && !loading && (
        <Card className="border-memBorder-primary">
          <CardContent className="p-4 space-y-2">
            <p className="text-sm font-medium">自动分类未开启</p>
            <p className="text-xs text-onSurface-default-tertiary">
              在「配置」页添加 <code>systemone</code>{" "}
              模型，新记忆就会自动打标签。
              无论是否开启，导入的记忆都会保留原有的分类。
            </p>
            <Button
              variant="outline"
              size="sm"
              onClick={() => router.push("/dashboard/configuration")}
            >
              打开配置
            </Button>
          </CardContent>
        </Card>
      )}

      {loading ? (
        <TableSkeleton rows={5} columns={3} />
      ) : (
        <div
          data-testid="categories-grid"
          className="grid grid-cols-1 md:grid-cols-3 gap-4"
        >
          {categories.map((cat) => {
            const count = counts[cat] ?? 0;
            return (
              <button
                key={cat}
                data-testid={`category-card-${cat}`}
                onClick={() =>
                  router.push(
                    `/dashboard/memories?categories=${encodeURIComponent(cat)}`,
                  )
                }
                className="text-left"
              >
                <Card className="border-memBorder-primary hover:bg-surface-default-tertiary transition-colors h-full">
                  <CardContent className="p-4">
                    <p className="text-sm font-medium mb-1">
                      {CATEGORY_LABELS[cat] ?? cat}
                    </p>
                    <p className="text-xs text-onSurface-default-tertiary">
                      {count} 条记忆
                    </p>
                  </CardContent>
                </Card>
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}
