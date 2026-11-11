"use client";

import { useEffect, useState } from "react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
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
import {
  MEMORY_ENDPOINTS,
  SERVER_ENDPOINTS,
  SYSTEMONE_ENDPOINTS,
} from "@/utils/api-endpoints";
import {
  buildProviderConfig,
  DEFAULT_EMBEDDING_DIMS,
  EXAMPLE_EMBEDDER_MODEL,
  EXAMPLE_MODEL,
  embedderKeyHint,
  getEffectiveConfig,
  readBaseUrl,
  resolveEmbedderApiKey,
} from "@/utils/self-hosted-config";
import { useAuth } from "@/hooks/use-auth";
import { useApiQuery } from "@/hooks/use-api-query";

type BundledProviders = {
  llm: string[];
  embedder: string[];
};

// Mirrors BUNDLED_SYSTEMONE_PROVIDERS in server-rs/crates/app/src/http/routes_config.rs.
// Kept local because /configure/providers is contract-locked by the golden tests.
// `systemone` is the SystemOne decision API (state + typed questions), not a chat model.
const SYSTEMONE_PROVIDERS = ["openai", "anthropic", "gemini", "systemone"];
const SYSTEMONE_DECISION = "systemone";
// No default on purpose: the decision provider is a vendor-specific API, so the operator
// has to name the endpoint rather than have memory text sent somewhere by default.
const SYSTEMONE_DEFAULT_BASE_URL = "";
// Radix Select cannot hold an empty string as a real choice.
const SYSTEMONE_OFF = "__off__";

export default function ConfigurationPage() {
  const { isAdmin } = useAuth();
  const [isSaving, setIsSaving] = useState(false);
  const [llmProvider, setLlmProvider] = useState("");
  const [llmModel, setLlmModel] = useState("");
  const [llmApiKey, setLlmApiKey] = useState("");
  const [llmBaseUrl, setLlmBaseUrl] = useState("");
  const [embedderProvider, setEmbedderProvider] = useState("");
  const [embedderModel, setEmbedderModel] = useState("");
  const [embedderApiKey, setEmbedderApiKey] = useState("");
  const [embedderBaseUrl, setEmbedderBaseUrl] = useState("");
  const [embeddingDims, setEmbeddingDims] = useState("");
  const [serverHasEmbedderKey, setServerHasEmbedderKey] = useState(false);
  // SystemOne = the optional classifier that tags memories with categories. Leaving the
  // provider on "Off" keeps classification disabled and stores categories: [].
  const [systemoneProvider, setSystemoneProvider] = useState("");
  const [systemoneModel, setSystemoneModel] = useState("");
  const [systemoneApiKey, setSystemoneApiKey] = useState("");
  const [systemoneBaseUrl, setSystemoneBaseUrl] = useState("");
  const [serverHasSystemoneKey, setServerHasSystemoneKey] = useState(false);
  // Only meaningful for the decision backend: how much probability of "does not apply"
  // is still counted as "applies".
  const [systemoneThreshold, setSystemoneThreshold] = useState("0.5");
  const [isTestingSystemone, setIsTestingSystemone] = useState(false);
  const [systemoneTest, setSystemoneTest] = useState<{
    ok: boolean;
    categories?: string[];
    error?: string;
  } | null>(null);
  const [port, setPort] = useState("");
  const [boundPort, setBoundPort] = useState<number | null>(null);
  const [isSavingPort, setIsSavingPort] = useState(false);

  const { data: config, isLoading: isPrefilling } = useApiQuery(
    async () => {
      const res = await api.get(MEMORY_ENDPOINTS.CONFIGURE);
      return getEffectiveConfig(res.data);
    },
    { errorToast: "Failed to load server configuration" },
  );

  const { data: providers } = useApiQuery<BundledProviders>(
    async () => {
      const res = await api.get<BundledProviders>(
        MEMORY_ENDPOINTS.CONFIGURE_PROVIDERS,
      );
      return res.data;
    },
    { errorToast: "Failed to load bundled providers" },
  );

  useEffect(() => {
    if (!config) return;
    setLlmProvider((current) => current || config.llm?.provider || "");
    setLlmModel((current) => current || config.llm?.config?.model || "");
    setLlmBaseUrl((current) => current || readBaseUrl(config.llm));
    setEmbedderProvider(
      (current) => current || config.embedder?.provider || "",
    );
    setEmbedderModel(
      (current) => current || config.embedder?.config?.model || "",
    );
    setEmbedderBaseUrl((current) => current || readBaseUrl(config.embedder));
    // A redacted value means the server holds one, so the field can stay blank without erasing it.
    setServerHasEmbedderKey(!!config.embedder?.config?.api_key);
    setSystemoneProvider(
      (current) => current || config.systemone?.provider || "",
    );
    setSystemoneModel(
      (current) => current || config.systemone?.config?.model || "",
    );
    setSystemoneBaseUrl((current) => current || readBaseUrl(config.systemone));
    setServerHasSystemoneKey(!!config.systemone?.config?.api_key);
    const threshold = config.systemone?.config?.noul_threshold;
    if (typeof threshold === "number") {
      setSystemoneThreshold((current) => current || String(threshold));
    }
    // The vector column width is the value that actually constrains what can be stored, so it is
    // the fallback when the embedder section does not state its own dimensions.
    const dims =
      config.embedder?.config?.embedding_dims ??
      config.vector_store?.config?.embedding_model_dims;
    setEmbeddingDims((current) => current || (dims ? String(dims) : ""));
  }, [config]);

  type DesktopSettings = {
    autostart: boolean;
    minimize_to_tray: boolean;
    start_silent: boolean;
    supported: { autostart: boolean; tray: boolean };
  };

  const { data: desktop, refetch: refetchDesktop } =
    useApiQuery<DesktopSettings>(
      async () => {
        const res = await api.get(SERVER_ENDPOINTS.DESKTOP);
        return res.data;
      },
      { errorToast: "无法加载桌面设置" },
    );

  const [isSavingDesktop, setIsSavingDesktop] = useState(false);

  const updateDesktop = async (patch: Partial<DesktopSettings>) => {
    setIsSavingDesktop(true);
    try {
      const res = await api.put(SERVER_ENDPOINTS.DESKTOP, patch);
      await refetchDesktop();
      // The login entry is written by the server; when Windows refuses it, the
      // response says why instead of the switch silently snapping back later.
      if (res.data?.error) {
        toast({
          title: "设置已保存，但没有生效",
          description: String(res.data.error),
          variant: "destructive",
        });
      }
    } catch (error) {
      toast({
        title: "保存桌面设置失败",
        description: getErrorMessage(error),
        variant: "destructive",
      });
      await refetchDesktop();
    } finally {
      setIsSavingDesktop(false);
    }
  };

  const { data: serverSettings, refetch: refetchServerSettings } = useApiQuery<{
    port: number | null;
    bound_port: number | null;
    min_port: number;
  }>(
    async () => {
      const res = await api.get(SERVER_ENDPOINTS.SETTINGS);
      return res.data;
    },
    { errorToast: "无法加载服务端设置" },
  );

  useEffect(() => {
    if (!serverSettings) return;
    setBoundPort(serverSettings.bound_port ?? null);
    setPort(
      (current) =>
        current || (serverSettings.port ? String(serverSettings.port) : ""),
    );
  }, [serverSettings]);

  const handleSavePort = async () => {
    const trimmed = port.trim();
    // Empty means "let the OS pick one", which the server stores as null.
    const value = trimmed === "" ? null : Number(trimmed);
    const isWholePort =
      value === null ||
      (Number.isInteger(value) && value >= 1024 && value <= 65535);
    if (!isWholePort) {
      toast({
        title: "端口无效",
        description: "请填 1024 到 65535 之间的整数，或留空。",
        variant: "destructive",
      });
      return;
    }
    setIsSavingPort(true);
    try {
      await api.put(SERVER_ENDPOINTS.SETTINGS, { port: value });
      await refetchServerSettings();
      toast({
        title: "端口已保存",
        description: "重启应用后生效。",
        variant: "success",
      });
    } catch (error) {
      toast({
        title: "保存端口失败",
        description: getErrorMessage(error),
        variant: "destructive",
      });
    } finally {
      setIsSavingPort(false);
    }
  };

  // Classification is best-effort at add time, so a wrong base_url looks exactly like
  // "no category applies". This asks the server to classify one fixed sentence and shows
  // whatever the provider said, verbatim.
  const handleTestSystemone = async () => {
    setIsTestingSystemone(true);
    setSystemoneTest(null);
    try {
      const res = await api.post(SYSTEMONE_ENDPOINTS.TEST, {});
      setSystemoneTest(res.data);
      const tagged = (res.data?.categories ?? []).join("、") || "(无)";
      if (res.data?.ok) {
        toast({
          title: "SystemOne 可用",
          description: "测试句被打上的标签：" + tagged,
          variant: "success",
        });
      } else {
        toast({
          title: "SystemOne 调用失败",
          description: String(res.data?.error ?? "未知错误"),
          variant: "destructive",
        });
      }
    } catch (error) {
      setSystemoneTest({ ok: false, error: getErrorMessage(error) });
      toast({
        title: "SystemOne 调用失败",
        description: getErrorMessage(error),
        variant: "destructive",
      });
    } finally {
      setIsTestingSystemone(false);
    }
  };

  const handleSave = async () => {
    setIsSaving(true);

    try {
      const llm = buildProviderConfig({
        provider: llmProvider,
        model: llmModel,
        apiKey: llmApiKey,
        baseUrl: llmBaseUrl,
      });
      const embedder = buildProviderConfig({
        provider: embedderProvider,
        model: embedderModel,
        apiKey: resolveEmbedderApiKey({
          embedderApiKey,
          llmApiKey,
          embedderProvider,
          llmProvider,
        }),
        baseUrl: embedderBaseUrl,
        embeddingDims: embeddingDims ? Number(embeddingDims) : undefined,
      });

      const newConfig: Record<string, unknown> = {
        version: "v1.1",
      };

      if (llm) {
        newConfig.llm = llm;
      }

      if (embedder) {
        newConfig.embedder = embedder;
      }

      // Always send the section: an explicit empty provider is how the operator turns
      // classification off, and leaving it out would silently keep the previous model.
      if (systemoneProvider === SYSTEMONE_DECISION) {
        // The decision API is not OpenAI-compatible: it wants a plain `base_url` and the
        // noul threshold, not a {provider}_base_url key.
        const decisionConfig: Record<string, unknown> = {
          model: systemoneModel || undefined,
          api_key: systemoneApiKey || undefined,
          think_mode: "auto",
        };
        if (systemoneBaseUrl.trim()) {
          decisionConfig.base_url = systemoneBaseUrl.trim();
        }
        const threshold = Number(systemoneThreshold);
        if (Number.isFinite(threshold)) {
          decisionConfig.noul_threshold = threshold;
        }
        newConfig.systemone = {
          provider: SYSTEMONE_DECISION,
          config: decisionConfig,
        };
      } else {
        newConfig.systemone = systemoneProvider
          ? buildProviderConfig({
              provider: systemoneProvider,
              model: systemoneModel,
              apiKey: resolveEmbedderApiKey({
                embedderApiKey: systemoneApiKey,
                llmApiKey,
                embedderProvider: systemoneProvider,
                llmProvider,
              }),
              baseUrl: systemoneBaseUrl,
            })
          : { provider: "", config: {} };
      }

      await api.post(MEMORY_ENDPOINTS.CONFIGURE, newConfig);
      toast({ title: "配置已保存", variant: "success" });
    } catch (error) {
      toast({
        title: "保存配置失败",
        description: getErrorMessage(error),
        variant: "destructive",
      });
    } finally {
      setIsSaving(false);
    }
  };

  return (
    <div className="space-y-6">
      <div className="space-y-1">
        <h1 className="text-xl font-semibold font-fustat">配置</h1>
        {isPrefilling && (
          <p className="text-sm text-onSurface-default-tertiary">
            正在读取服务端当前生效的配置…
          </p>
        )}
      </div>

      <Card className="border-memBorder-primary">
        <CardHeader>
          <CardTitle className="text-sm">LLM 模型</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1">
              <Label className="text-xs">Provider</Label>
              <Select
                value={llmProvider}
                onValueChange={(value) => {
                  // Radix emits "" while the option list is still empty, because it cannot resolve
                  // the value the server just sent. Taking that literally would erase the prefilled
                  // provider and leave the form unable to save.
                  if (!value) return;
                  setLlmProvider(value);
                  setLlmApiKey("");
                }}
                disabled={!isAdmin || !providers}
              >
                <SelectTrigger>
                  <SelectValue placeholder="选择 provider">
                    {llmProvider || undefined}
                  </SelectValue>
                </SelectTrigger>
                <SelectContent>
                  {providers?.llm.map((name) => (
                    <SelectItem key={name} value={name}>
                      {name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="space-y-1">
              <Label className="text-xs">模型</Label>
              <Input
                placeholder={
                  EXAMPLE_MODEL[llmProvider] ?? "gpt-4.1-nano-2025-04-14"
                }
                value={llmModel}
                onChange={(e) => setLlmModel(e.target.value)}
                disabled={!isAdmin}
              />
            </div>
          </div>
          <div className="space-y-1">
            <Label className="text-xs">API 密钥</Label>
            <Input
              type="password"
              placeholder="sk-..."
              value={llmApiKey}
              onChange={(e) => setLlmApiKey(e.target.value)}
              disabled={!isAdmin}
            />
          </div>
          <div className="space-y-1">
            <Label className="text-xs">Base URL</Label>
            <Input
              placeholder="留空则用该 provider 的默认值"
              value={llmBaseUrl}
              onChange={(e) => setLlmBaseUrl(e.target.value)}
              disabled={!isAdmin}
            />
            <p className="text-xs text-onSurface-default-tertiary">
              用于代理、网关或自建端点。
            </p>
          </div>
        </CardContent>
      </Card>

      <Card className="border-memBorder-primary">
        <CardHeader>
          <CardTitle className="text-sm">嵌入模型</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1">
              <Label className="text-xs">Provider</Label>
              <Select
                value={embedderProvider}
                onValueChange={(value) => {
                  // Same as the LLM section: Radix emits "" before the options exist.
                  if (!value) return;
                  setEmbedderProvider(value);
                  // A key belongs to the provider that issued it, so the stored one no longer
                  // applies. Clearing the flag is what turns the hint below into "required".
                  setServerHasEmbedderKey(false);
                  setEmbedderApiKey("");
                  // The vector column is fixed-width, so a provider switch that keeps the old
                  // width produces vectors the table cannot hold. Follow the new default.
                  const next = DEFAULT_EMBEDDING_DIMS[value];
                  if (next) setEmbeddingDims(String(next));
                }}
                disabled={!isAdmin || !providers}
              >
                <SelectTrigger>
                  <SelectValue placeholder="选择 provider">
                    {embedderProvider || undefined}
                  </SelectValue>
                </SelectTrigger>
                <SelectContent>
                  {providers?.embedder.map((name) => (
                    <SelectItem key={name} value={name}>
                      {name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="space-y-1">
              <Label className="text-xs">模型</Label>
              <Input
                placeholder={
                  EXAMPLE_EMBEDDER_MODEL[embedderProvider] ??
                  "text-embedding-3-small"
                }
                value={embedderModel}
                onChange={(e) => setEmbedderModel(e.target.value)}
                disabled={!isAdmin}
              />
            </div>
          </div>
          <div className="space-y-1">
            <Label className="text-xs">API 密钥</Label>
            <Input
              type="password"
              placeholder={
                serverHasEmbedderKey ? "留空表示沿用已保存的密钥" : "sk-..."
              }
              value={embedderApiKey}
              onChange={(e) => setEmbedderApiKey(e.target.value)}
              disabled={!isAdmin}
            />
            <p className="text-xs text-onSurface-default-tertiary">
              {embedderKeyHint({
                serverHasKey: serverHasEmbedderKey,
                embedderProvider,
                llmProvider,
              })}
            </p>
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1">
              <Label className="text-xs">维度</Label>
              <Input
                type="number"
                inputMode="numeric"
                placeholder="1536"
                value={embeddingDims}
                onChange={(e) => setEmbeddingDims(e.target.value)}
                disabled={!isAdmin}
              />
              <p className="text-xs text-onSurface-default-tertiary">
                必须与模型一致。修改它需要先清空记忆。
              </p>
            </div>
            <div className="space-y-1">
              <Label className="text-xs">Base URL</Label>
              <Input
                placeholder="留空则用该 provider 的默认值"
                value={embedderBaseUrl}
                onChange={(e) => setEmbedderBaseUrl(e.target.value)}
                disabled={!isAdmin}
              />
            </div>
          </div>
        </CardContent>
      </Card>

      <Card className="border-memBorder-primary">
        <CardHeader className="flex flex-row items-center justify-between space-y-0">
          <CardTitle className="text-sm">分类（SystemOne）</CardTitle>
          <Button
            data-testid="systemone-test"
            variant="outline"
            size="sm"
            onClick={() => void handleTestSystemone()}
            disabled={!isAdmin || isTestingSystemone || !systemoneProvider}
          >
            {isTestingSystemone ? "测试中…" : "测试"}
          </Button>
        </CardHeader>
        <CardContent className="space-y-3">
          <p className="text-xs text-onSurface-default-tertiary">
            可选。配置后，每条新记忆都会被打上 15
            个预设分类中的一个或多个，「分类」页据此归组。保持「关闭」则记忆不带标签。选{" "}
            <code>systemone</code> 表示使用 SystemOne decision API，其余选项都是
            OpenAI 兼容的对话模型。
          </p>
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1">
              <Label className="text-xs">Provider</Label>
              <Select
                value={systemoneProvider || SYSTEMONE_OFF}
                onValueChange={(value) => {
                  if (!value) return;
                  const next = value === SYSTEMONE_OFF ? "" : value;
                  setSystemoneProvider(next);
                  // A key belongs to the provider that issued it; clearing the flag is
                  // what turns the hint below into "required".
                  setServerHasSystemoneKey(false);
                  setSystemoneApiKey("");
                }}
                disabled={!isAdmin}
              >
                <SelectTrigger data-testid="systemone-provider">
                  <SelectValue placeholder="关闭">
                    {systemoneProvider || "关闭（不分类）"}
                  </SelectValue>
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value={SYSTEMONE_OFF}>关闭（不分类）</SelectItem>
                  {SYSTEMONE_PROVIDERS.map((name) => (
                    <SelectItem key={name} value={name}>
                      {name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="space-y-1">
              <Label className="text-xs">模型</Label>
              <Input
                data-testid="systemone-model"
                placeholder={
                  systemoneProvider === SYSTEMONE_DECISION
                    ? "startlux-decision-27b-autothink"
                    : (EXAMPLE_MODEL[systemoneProvider] ?? "gpt-4o-mini")
                }
                value={systemoneModel}
                onChange={(e) => setSystemoneModel(e.target.value)}
                disabled={!isAdmin || !systemoneProvider}
              />
            </div>
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1">
              <Label className="text-xs">API 密钥</Label>
              <Input
                type="password"
                placeholder={
                  serverHasSystemoneKey ? "留空表示沿用已保存的密钥" : "sk-..."
                }
                value={systemoneApiKey}
                onChange={(e) => setSystemoneApiKey(e.target.value)}
                disabled={!isAdmin || !systemoneProvider}
              />
              <p className="text-xs text-onSurface-default-tertiary">
                {systemoneProvider
                  ? embedderKeyHint({
                      serverHasKey: serverHasSystemoneKey,
                      embedderProvider: systemoneProvider,
                      llmProvider,
                    })
                  : "关闭分类时不需要密钥。"}
              </p>
            </div>
            <div className="space-y-1">
              <Label className="text-xs">Base URL</Label>
              <Input
                data-testid="systemone-base-url"
                placeholder={
                  systemoneProvider === SYSTEMONE_DECISION
                    ? SYSTEMONE_DEFAULT_BASE_URL
                    : "留空则用该 provider 的默认值"
                }
                value={systemoneBaseUrl}
                onChange={(e) => setSystemoneBaseUrl(e.target.value)}
                disabled={!isAdmin || !systemoneProvider}
              />
            </div>
          </div>
          {systemoneProvider === SYSTEMONE_DECISION && (
            <div className="max-w-[200px] space-y-1">
              <Label className="text-xs">Noul 阈值</Label>
              <Input
                data-testid="systemone-threshold"
                inputMode="decimal"
                placeholder="0.5"
                value={systemoneThreshold}
                onChange={(e) =>
                  setSystemoneThreshold(e.target.value.replace(/[^0-9.]/g, ""))
                }
                disabled={!isAdmin}
              />
              <p className="text-xs text-onSurface-default-tertiary">
                模型给出的「不适用」概率低于此值时，该分类才算命中。调高会打得更多，调低会打得更少。
              </p>
            </div>
          )}
          {systemoneTest && (
            <div
              data-testid="systemone-test-result"
              className={
                "rounded border p-3 text-xs font-mono whitespace-pre-wrap break-words " +
                (systemoneTest.ok
                  ? "border-memBorder-primary text-onSurface-default-secondary"
                  : "border-red-500/40 text-red-500")
              }
            >
              {systemoneTest.ok
                ? "成功 - 测试句被打上的标签：" +
                  ((systemoneTest.categories ?? []).join("、") || "(无)")
                : "失败 - " + systemoneTest.error}
            </div>
          )}
        </CardContent>
      </Card>

      <p className="text-xs text-onSurface-default-tertiary">
        需要别的 provider？把它的依赖装进来、重新构建，并把它加进内置列表。参见{" "}
        <a
          href="https://docs.mem0.ai/open-source/setup#supported-providers"
          target="_blank"
          rel="noopener noreferrer"
          className="underline underline-offset-4 hover:text-onSurface-default-primary"
        >
          配置指南
        </a>
        .
      </p>

      {isAdmin && (
        <Button onClick={handleSave} disabled={isSaving}>
          {isSaving ? "正在保存…" : "保存配置"}
        </Button>
      )}

      <Card className="border-memBorder-primary">
        <CardHeader>
          <CardTitle className="text-sm">服务端</CardTitle>
        </CardHeader>
        <CardContent className="space-y-3">
          <div className="flex items-end gap-3">
            <div className="max-w-[200px] space-y-1">
              <Label className="text-xs">端口</Label>
              <Input
                data-testid="server-port-input"
                inputMode="numeric"
                placeholder="自动"
                value={port}
                onChange={(e) => setPort(e.target.value.replace(/\D/g, ""))}
                disabled={!isAdmin}
              />
            </div>
            <Button
              data-testid="server-port-save"
              variant="outline"
              onClick={handleSavePort}
              disabled={!isAdmin || isSavingPort}
            >
              {isSavingPort ? "正在保存…" : "保存端口"}
            </Button>
          </div>
          <p className="text-xs text-onSurface-default-tertiary">
            {boundPort ? `当前正在 127.0.0.1:${boundPort} 上提供服务。` : ""}
            端口在下次启动应用时生效，正在运行的服务仍用当前端口。留空则每次启动由系统挑一个空闲端口。若启动时保存的端口已被占用，会改用一个空闲端口。
          </p>
        </CardContent>
      </Card>

      <Card className="border-memBorder-primary">
        <CardHeader>
          <CardTitle className="text-sm">桌面</CardTitle>
        </CardHeader>
        <CardContent className="space-y-4">
          {[
            {
              key: "autostart" as const,
              label: "开机自启",
              hint: desktop?.supported.autostart
                ? "把本应用加入开机启动项，随系统一起启动。"
                : "仅 Windows 版本可用。",
              disabled: !desktop?.supported.autostart,
            },
            {
              key: "minimize_to_tray" as const,
              label: "关闭窗口后继续在托盘运行",
              hint: "关闭窗口只是隐藏，不会退出。要退出请用托盘菜单里的「退出」。",
              disabled: false,
            },
            {
              key: "start_silent" as const,
              label: "启动时最小化",
              hint: "启动时不弹窗，需要时点托盘图标即可。",
              disabled: false,
            },
          ].map((item) => (
            <div
              key={item.key}
              className="flex items-start justify-between gap-6"
            >
              <div className="space-y-0.5">
                <Label className="text-xs">{item.label}</Label>
                <p className="text-xs text-onSurface-default-tertiary">
                  {item.hint}
                </p>
              </div>
              <Switch
                data-testid={`desktop-${item.key}`}
                checked={Boolean(desktop?.[item.key])}
                disabled={!isAdmin || isSavingDesktop || item.disabled}
                onCheckedChange={(checked) =>
                  void updateDesktop({ [item.key]: checked })
                }
              />
            </div>
          ))}
        </CardContent>
      </Card>
    </div>
  );
}
