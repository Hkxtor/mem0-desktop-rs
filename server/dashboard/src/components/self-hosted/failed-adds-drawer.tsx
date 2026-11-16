"use client";

import { useEffect, useState } from "react";
import { AlertTriangle, RotateCw, Trash2 } from "lucide-react";
import { format, formatDistanceToNow } from "date-fns";
import { zhCN } from "date-fns/locale";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import {
  Sheet,
  SheetContent,
  SheetDescription,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet";
import { toast } from "@/components/ui/use-toast";
import { getErrorMessage } from "@/lib/error-message";
import { api } from "@/utils/api";
import { FAILED_ADD_ENDPOINTS } from "@/utils/api-endpoints";
import { ApiFailedAdd, ApiFailedAdds, ApiRetryResult } from "@/types/api";

/** Long enough to recognize the input, short enough that a drawer of fifty stays readable. */
const PREVIEW_CHARS = 200;

/**
 * Why an add kept its input, in the words the drawer shows.
 *
 * These are codes stored with the row, not sentences: the provider's own message lives in the
 * application log, and a translated copy of it in the database would drift from the source.
 */
const REASONS: Record<string, { label: string; hint: string }> = {
  provider_error: {
    label: "模型调用失败",
    hint: "限速、额度用尽或上游故障。修好之后重试即可，原文还在。",
  },
  unparsed_answer: {
    label: "回答无法解析",
    hint: "模型没有给出可用的 JSON，一条完整的记忆都没取到。",
  },
  embed_failed: {
    label: "嵌入失败",
    hint: "记忆取到了，但拿不到向量。重试只补嵌入，不会重新提问模型。",
  },
  add_failed: {
    label: "添加失败",
    hint: "存储环节出错。修好之后重试。",
  },
};

const MAX_BATCH = 20;

interface FailedAddsDrawerProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Called after a retry stores something, so the list behind the drawer can refresh. */
  onStored?: () => void;
}

/** "5 分钟前" — the drawer is wide, so the spelled-out form fits. */
function shortAgo(value: string): string {
  const then = new Date(value);
  if (Number.isNaN(then.getTime())) return "--";
  return formatDistanceToNow(then, { addSuffix: true, locale: zhCN });
}

function preview(text: string): string {
  return text.length > PREVIEW_CHARS
    ? `${text.slice(0, PREVIEW_CHARS)}…`
    : text;
}

function scopeOf(row: ApiFailedAdd): string {
  const parts = [
    row.user_id && `user:${row.user_id}`,
    row.agent_id && `agent:${row.agent_id}`,
    row.run_id && `run:${row.run_id}`,
  ].filter(Boolean);
  return parts.length > 0 ? parts.join(" · ") : "-";
}

/**
 * The retry queue: adds that stored nothing but kept their input.
 *
 * A drawer rather than a page because it is a side path off Memories, and listing the rows
 * matters more than discovering them: the text here is memory content, and nothing should be
 * replayed without being seen first.
 */
export default function FailedAddsDrawer({
  open,
  onOpenChange,
  onStored,
}: FailedAddsDrawerProps) {
  const [rows, setRows] = useState<ApiFailedAdd[]>([]);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(false);
  const [running, setRunning] = useState(false);
  const [progress, setProgress] = useState({ done: 0, total: 0 });
  const [expanded, setExpanded] = useState<number[]>([]);

  const load = async () => {
    setLoading(true);
    try {
      const res = await api.get<ApiFailedAdds>(FAILED_ADD_ENDPOINTS.BASE);
      const items = Array.isArray(res.data?.items) ? res.data.items : [];
      setRows(items);
      setTotal(Number(res.data?.total ?? items.length));
    } catch (err) {
      toast({
        title: "无法加载待重试列表",
        description: getErrorMessage(err),
        variant: "destructive",
      });
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    if (open) void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  const runRetry = async (ids?: number[]) => {
    if (running) return;
    setRunning(true);
    setProgress({ done: 0, total: ids?.length ?? total });
    let stored = 0;
    let failed = 0;
    let remaining = 0;
    let stopped: string | null = null;
    try {
      // One batch per call, oldest first, until the queue stops shrinking. Same shape as batch
      // classification: a long run stays visible and stoppable instead of hanging.
      for (;;) {
        const res = await api.post<ApiRetryResult>(FAILED_ADD_ENDPOINTS.RETRY, {
          ...(ids ? { ids } : {}),
          limit: MAX_BATCH,
        });
        const chunk = res.data;
        stored += Number(chunk?.stored ?? 0);
        failed += Number(chunk?.failed ?? 0);
        remaining = Number(chunk?.remaining ?? 0);
        stopped = chunk?.stopped_reason ?? null;
        setProgress((p) => ({
          done: p.done + Number(chunk?.retried ?? 0),
          total: p.total,
        }));
        if (ids) break;
        if (stopped) break;
        if (Number(chunk?.retried ?? 0) === 0) break;
        if (remaining <= 0) break;
      }
    } catch (err) {
      stopped = getErrorMessage(err);
    } finally {
      setRunning(false);
      const parts = [`存下 ${stored} 条`];
      if (failed > 0) parts.push(`${failed} 条仍然失败`);
      if (remaining > 0) parts.push(`还剩 ${remaining} 条`);
      toast({
        title: stopped
          ? "重试已中止"
          : stored > 0
            ? "重试完成"
            : "重试没有存下任何记忆",
        description: stopped
          ? `${parts.join(" · ")}. ${stopped}`
          : parts.join(" · "),
        variant: stopped || stored === 0 ? "destructive" : "success",
      });
      if (stored > 0) onStored?.();
      await load();
    }
  };

  const deleteAll = async () => {
    try {
      const res = await api.delete(FAILED_ADD_ENDPOINTS.BASE);
      toast({
        title: "已清空待重试列表",
        description: `${Number(res.data?.removed ?? 0)} 条原文已删除`,
        variant: "success",
      });
      await load();
    } catch (err) {
      toast({
        title: "清空失败",
        description: getErrorMessage(err),
        variant: "destructive",
      });
    }
  };

  return (
    <Sheet open={open} onOpenChange={onOpenChange}>
      <SheetContent className="w-full sm:max-w-2xl overflow-y-auto">
        <SheetHeader>
          <SheetTitle data-testid="failed-adds-title">
            待重试的添加（{total}）
          </SheetTitle>
          <SheetDescription>
            这些添加没有存下任何记忆，但原文已经保留。修好模型或密钥之后重试即可，
            不需要客户端重新提交。
          </SheetDescription>
        </SheetHeader>

        <div className="flex flex-wrap items-center gap-2 mt-4">
          {running ? (
            <>
              <Button size="sm" disabled data-testid="failed-adds-progress">
                正在重试 {progress.done}
                {progress.total > 0 ? ` / ${progress.total}` : ""}…
              </Button>
              <span className="text-xs text-onSurface-default-tertiary">
                当前批次跑完后停止
              </span>
            </>
          ) : (
            <Button
              size="sm"
              data-testid="failed-adds-retry-all"
              disabled={rows.length === 0}
              onClick={() => void runRetry()}
            >
              <RotateCw className="size-3.5 mr-1" />
              全部重试
            </Button>
          )}
          <Button
            size="sm"
            variant="outline"
            data-testid="failed-adds-delete-all"
            disabled={rows.length === 0 || running}
            onClick={() => void deleteAll()}
          >
            <Trash2 className="size-3.5 mr-1" />
            全部删除
          </Button>
        </div>

        {loading ? (
          <p className="text-sm text-onSurface-default-tertiary mt-6">
            加载中…
          </p>
        ) : rows.length === 0 ? (
          <p
            data-testid="failed-adds-empty"
            className="text-sm text-onSurface-default-tertiary mt-6"
          >
            没有待重试的添加。添加失败时，原文会出现在这里。
          </p>
        ) : (
          <ul className="mt-4 space-y-2">
            {rows.map((row) => {
              const reason = REASONS[row.reason] ?? {
                label: row.reason,
                hint: "详情见日志页。",
              };
              const isOpen = expanded.includes(row.id);
              const show = (text: string) => (isOpen ? text : preview(text));
              return (
                <li
                  key={row.id}
                  data-testid={`failed-add-${row.id}`}
                  className="rounded-md border border-memBorder-primary p-3 space-y-2"
                >
                  <div className="flex flex-wrap items-center gap-2 text-xs text-onSurface-default-tertiary">
                    <span
                      title={format(new Date(row.created_at), "PPpp", {
                        locale: zhCN,
                      })}
                    >
                      {shortAgo(row.created_at)}
                    </span>
                    <span>·</span>
                    <span className="truncate max-w-[220px]">
                      {scopeOf(row)}
                    </span>
                    <Badge variant="outline" className="px-1 py-0">
                      {reason.label}
                    </Badge>
                    {row.attempts > 1 && (
                      <Badge variant="outline" className="px-1 py-0">
                        已尝试 {row.attempts} 次
                      </Badge>
                    )}
                  </div>

                  <div className="space-y-1">
                    {row.messages.map((m, i) => (
                      <p
                        key={i}
                        className="text-sm whitespace-pre-wrap break-words"
                      >
                        <span className="text-onSurface-default-tertiary mr-1">
                          {m.role}:
                        </span>
                        {show(m.content)}
                      </p>
                    ))}
                    {row.items.length > 0 && (
                      <p className="text-xs text-onSurface-default-secondary">
                        已抽出但没存下：{row.items.map(show).join(" / ")}
                      </p>
                    )}
                  </div>

                  {row.last_error && (
                    <p className="text-xs text-onSurface-default-tertiary flex items-start gap-1 min-w-0">
                      <AlertTriangle className="size-3 mt-0.5 shrink-0" />
                      {/* The provider's message can be long and has no spaces to wrap on. */}
                      <span className="break-all min-w-0">
                        {row.last_error}
                      </span>
                    </p>
                  )}

                  <div className="flex items-center gap-2">
                    <Button
                      size="sm"
                      variant="outline"
                      data-testid={`failed-add-retry-${row.id}`}
                      disabled={running}
                      onClick={() => void runRetry([row.id])}
                    >
                      重试这一条
                    </Button>
                    <button
                      className="text-xs text-onSurface-default-tertiary underline underline-offset-4"
                      onClick={() =>
                        setExpanded((cur) =>
                          cur.includes(row.id)
                            ? cur.filter((id) => id !== row.id)
                            : [...cur, row.id],
                        )
                      }
                    >
                      {isOpen ? "收起原文" : "展开原文"}
                    </button>
                  </div>
                </li>
              );
            })}
          </ul>
        )}

        <p className="text-xs text-onSurface-default-tertiary mt-6">
          这些记录保留 14 天，最多 500
          条，超过后自动清理最旧的。重试成功后记录立刻删除——
          原文此后由这条记忆的「输入」保存，不再存第二份。
        </p>
      </SheetContent>
    </Sheet>
  );
}
