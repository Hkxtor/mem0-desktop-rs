"use client";

import { useState } from "react";
import { AlertTriangle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { toast } from "@/components/ui/use-toast";
import {
  AlertDialog,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import { getErrorMessage } from "@/lib/error-message";
import { api } from "@/utils/api";
import { EXPORT_ENDPOINTS, MEMORY_ENDPOINTS } from "@/utils/api-endpoints";
import { useApiQuery } from "@/hooks/use-api-query";
import {
  backupFilename,
  downloadJson,
  fetchAllMemories,
} from "@/utils/export-all";

/** The word the operator has to type. Matched exactly: no trim, no case folding. */
const CONFIRM_WORD = "RESET";

type Phase = "idle" | "backing-up" | "clearing";

/**
 * Clear every memory, with the two guards that make an irreversible action survivable.
 *
 * 1. Typing `RESET` is required, so a stray double-click on a card cannot empty the library.
 * 2. A **complete** backup is downloaded first, and the reset only fires after it succeeded.
 *    The order is the whole point: clearing before backing up is not a backup at all, and if
 *    the backup fails the operator is told and the data is left alone.
 *
 * This exists because changing embedding model or dimensions is the one thing that requires
 * emptying the store — export, reset, reconfigure, re-import — and that path used to need curl.
 */
export const ResetMemoriesCard = () => {
  // The count comes from `/export?limit=0`, which answers with the collection total. It is the
  // same number the clear button announces, so the two can never disagree.
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
  const [open, setOpen] = useState(false);
  const [typed, setTyped] = useState("");
  const [phase, setPhase] = useState<Phase>("idle");

  const busy = phase !== "idle";
  const unlocked = typed === CONFIRM_WORD;

  const close = () => {
    if (busy) return;
    setOpen(false);
    setTyped("");
  };

  const handleConfirm = async () => {
    if (!unlocked || busy) return;
    // How many rows this press is about to delete. Read from the same source the page shows, so
    // the toast cannot claim a number the operator never saw.
    const announced = totalInStore;
    try {
      setPhase("backing-up");
      // Every page, not the first one. A backup that stops at the page cap would be worse than
      // no backup: the operator would believe the data is safe.
      const { memories } = await fetchAllMemories();
      if (memories.length !== announced) {
        // Someone added or deleted while the dialog was open. Downloading anyway is still a
        // valid backup, but the count the operator agreed to is no longer what will be cleared,
        // so this stops rather than guessing.
        throw new Error(
          `记忆数量在确认期间发生了变化（${announced} → ${memories.length}），已中止。` +
            `备份文件已下载，请重新确认。`,
        );
      }
      downloadJson(
        {
          exported_at: new Date().toISOString(),
          total: memories.length,
          memories,
        },
        backupFilename(),
      );

      setPhase("clearing");
      await api.post(MEMORY_ENDPOINTS.RESET);

      toast({
        title: `已清空 ${announced.toLocaleString()} 条记忆`,
        description: "备份文件已下载，可用它在改完嵌入模型后重新导入。",
        variant: "success",
      });
      setOpen(false);
      setTyped("");
      void refetch();
    } catch (error) {
      // The reset either did not run or the server refused it. Never suggest that data was lost.
      toast({
        title: "清空未完成",
        description: getErrorMessage(error),
        variant: "destructive",
      });
    } finally {
      setPhase("idle");
    }
  };

  return (
    <Card className="border-memBorder-secondary bg-surface-default-primary">
      <CardHeader>
        <CardTitle className="text-base">危险操作</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        <div className="space-y-1">
          <Label className="text-xs">清空所有记忆</Label>
          <p className="text-xs text-onSurface-default-tertiary">
            删除本机保存的全部记忆、它们的输入记录和待重试队列。换嵌入模型或维度时必须先清空，
            因为旧向量与新模型不在同一个向量空间里。清空前会自动下载一份完整备份，备份失败则不会清空。
          </p>
        </div>
        <AlertDialog
          open={open}
          onOpenChange={(next) => {
            if (next) setOpen(true);
            else close();
          }}
        >
          <AlertDialogTrigger asChild>
            <Button
              variant="outline"
              className="border-rose-300 text-rose-700 hover:bg-rose-50 dark:border-rose-900 dark:text-rose-300 dark:hover:bg-rose-950/40"
              disabled={isLoading || totalInStore === 0}
              data-testid="reset-memories-open"
            >
              <AlertTriangle className="size-4 mr-2" />
              清空所有记忆
            </Button>
          </AlertDialogTrigger>
          <AlertDialogContent>
            <AlertDialogHeader>
              <AlertDialogTitle>
                清空 {totalInStore.toLocaleString()} 条记忆？
              </AlertDialogTitle>
              <AlertDialogDescription asChild>
                <div className="space-y-3 text-sm text-onSurface-default-secondary">
                  <p>
                    这件事无法撤销。清空前会先自动下载一份完整的 JSON
                    备份；如果备份取不到， 清空不会执行。
                  </p>
                  <p>
                    请先确认嵌入模型已经是新的配置，之后用导出的文件重新导入。
                  </p>
                  <div className="space-y-1">
                    <Label htmlFor="reset-confirm" className="text-xs">
                      输入 {CONFIRM_WORD} 以确认
                    </Label>
                    <Input
                      id="reset-confirm"
                      value={typed}
                      onChange={(event) => setTyped(event.target.value)}
                      placeholder={CONFIRM_WORD}
                      autoComplete="off"
                      spellCheck={false}
                      disabled={busy}
                      data-testid="reset-confirm-input"
                    />
                  </div>
                </div>
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogCancel disabled={busy} data-testid="reset-cancel">
                取消
              </AlertDialogCancel>
              <Button
                variant="destructive"
                onClick={(event) => {
                  // Keep the dialog open for the whole backup + reset: closing on click would
                  // hide the progress and any error behind it.
                  event.preventDefault();
                  void handleConfirm();
                }}
                disabled={!unlocked || busy}
                data-testid="reset-confirm-button"
              >
                {phase === "backing-up"
                  ? "正在备份…"
                  : phase === "clearing"
                    ? "正在清空…"
                    : "下载备份并清空"}
              </Button>
            </AlertDialogFooter>
          </AlertDialogContent>
        </AlertDialog>
      </CardContent>
    </Card>
  );
};
