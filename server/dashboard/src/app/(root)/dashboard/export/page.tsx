"use client";

import { useRef, useState } from "react";
import { Download, Upload } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { toast } from "@/components/ui/use-toast";
import { getErrorMessage } from "@/lib/error-message";
import { api } from "@/utils/api";
import { EXPORT_ENDPOINTS } from "@/utils/api-endpoints";
import { useApiQuery } from "@/hooks/use-api-query";
import {
  backupFilename,
  downloadJson,
  fetchAllMemories,
} from "@/utils/export-all";

export default function ExportPage() {
  const [isExporting, setIsExporting] = useState(false);
  const [isImporting, setIsImporting] = useState(false);
  const fileInput = useRef<HTMLInputElement>(null);

  // The count used to come from `/memories?top_k=1000`, so on a larger library the page showed
  // the cap rather than the collection. `/export` now reports the real total.
  const {
    data: totalInStore = 0,
    isLoading,
    refetch,
  } = useApiQuery<number>(
    async () => {
      const res = await api.get(EXPORT_ENDPOINTS.EXPORT, {
        params: { format: "json", limit: 0 },
      });
      const total = res.data?.total_in_store;
      return typeof total === "number" ? total : 0;
    },
    { errorToast: "无法统计记忆数量", initialData: 0 },
  );

  const hasMemories = totalInStore > 0;

  const handleExport = async () => {
    setIsExporting(true);

    try {
      // Every page, not just the first: a capped export is a silent partial backup, and this
      // file is what people restore from after a reset.
      const { memories, totalInStore: total } = await fetchAllMemories();
      downloadJson(
        { exported_at: new Date().toISOString(), total, memories },
        backupFilename(),
      );

      toast({
        title: `已导出 ${memories.length.toLocaleString()} 条记忆`,
        description:
          memories.length === total
            ? undefined
            : `库中共 ${total.toLocaleString()} 条，本次全部包含在内。`,
        variant: "success",
      });
    } catch (error) {
      toast({
        title: "导出失败",
        description: getErrorMessage(error),
        variant: "destructive",
      });
    } finally {
      setIsExporting(false);
    }
  };

  /**
   * Re-import a file this page (or another install) exported as JSON.
   *
   * The server re-embeds every memory with the currently configured embedder, so this is also the
   * second half of switching embedding models: export, POST /reset, change the dimensions in
   * Configuration, then import the same file back.
   */
  const handleImportFile = async (file: File) => {
    setIsImporting(true);
    try {
      const text = await file.text();
      const parsed = JSON.parse(text) as { memories?: unknown } | unknown[];
      const payload = Array.isArray(parsed) ? { memories: parsed } : parsed;

      const res = await api.post(EXPORT_ENDPOINTS.IMPORT, payload);
      const result = res.data as {
        imported: number;
        skipped: number;
        total_in_file: number;
      };
      toast({
        title: `已导入 ${result.imported.toLocaleString()} / ${result.total_in_file.toLocaleString()} 条记忆`,
        description:
          result.skipped > 0
            ? `其中 ${result.skipped} 条重新嵌入失败，已跳过。`
            : "所有记忆都已用当前模型重新嵌入。",
        variant: result.skipped > 0 ? "destructive" : "success",
      });
      void refetch();
    } catch (error) {
      toast({
        title: "导入失败",
        description: getErrorMessage(error),
        variant: "destructive",
      });
    } finally {
      setIsImporting(false);
      if (fileInput.current) fileInput.current.value = "";
    }
  };

  return (
    <div className="space-y-6">
      <div className="space-y-1">
        <h1 className="text-xl font-semibold font-fustat">导出</h1>
        <p className="text-sm text-onSurface-default-tertiary">
          下载本机保存的全部记忆，JSON
          格式。数据不会离开这台机器——导出内容与「记忆」页列出的是同一份数据。
        </p>
        <p className="text-sm text-onSurface-default-tertiary">
          导出会读取全部记忆，不受列表分页影响；只有 JSON
          能原样导入回来，所以它是唯一提供的格式。
        </p>
      </div>

      <Card className="border-memBorder-primary">
        <CardContent className="flex flex-wrap items-center justify-between gap-4 p-4">
          <div>
            <Label className="text-xs text-onSurface-default-tertiary">
              待导出
            </Label>
            <p className="text-sm">
              {isLoading
                ? "正在统计记忆数量…"
                : `${totalInStore.toLocaleString()} 条记忆`}
            </p>
          </div>
          <Button
            onClick={handleExport}
            disabled={isExporting || isLoading || !hasMemories}
          >
            <Download className="size-4" />
            {isExporting ? "正在准备…" : "导出全部记忆"}
          </Button>
        </CardContent>
      </Card>

      <Card className="border-memBorder-primary">
        <CardHeader className="pb-2">
          <CardTitle className="text-sm">导入</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <p className="text-sm text-onSurface-default-secondary">
            恢复一份 JSON
            导出。每条记忆都会用当前配置的模型重新嵌入，因此这也是切换模型的后半程：先导出、再重置、改嵌入维度，最后导入。
          </p>
          <input
            ref={fileInput}
            type="file"
            accept="application/json,.json"
            className="hidden"
            onChange={(event) => {
              const file = event.target.files?.[0];
              if (file) void handleImportFile(file);
            }}
          />
          <Button
            variant="outline"
            onClick={() => fileInput.current?.click()}
            disabled={isImporting}
          >
            <Upload className="size-4" />
            {isImporting ? "正在导入…" : "导入 JSON 导出文件"}
          </Button>
        </CardContent>
      </Card>

      {!isLoading && !hasMemories && (
        <p className="text-sm text-onSurface-default-tertiary">
          目前还没有可导出的内容。先添加一条记忆，或让 API
          指向本服务，之后就会出现在这里。
        </p>
      )}
    </div>
  );
}
