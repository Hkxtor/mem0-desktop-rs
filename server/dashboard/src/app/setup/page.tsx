"use client";

import { useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { Check, Copy } from "lucide-react";
import { CopyToClipboard } from "react-copy-to-clipboard";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Card, CardContent } from "@/components/ui/card";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { useAuth } from "@/hooks/use-auth";
import { apiUrl, displayApiUrl } from "@/lib/api-url";
import { getErrorMessage } from "@/lib/error-message";
import { useSetupStatus } from "@/lib/use-setup-status";
import { cn } from "@/lib/utils";
import { api } from "@/utils/api";
import {
  API_KEY_ENDPOINTS,
  AUTH_ENDPOINTS,
  MEMORY_ENDPOINTS,
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
import { isValidEmail } from "@/lib/validators";

type BundledProviders = {
  llm: string[];
  embedder: string[];
};

const STEPS = ["管理员账号", "Provider", "API 密钥", "使用场景", "快速验证"];
const STEP_TITLES = [
  "创建管理员账号",
  "确认 provider 配置",
  "你的 API 密钥",
  "说说你的使用场景",
  "验证配置是否可用",
];
const SUPPORTED_PROVIDERS_URL =
  "https://docs.mem0.ai/open-source/setup#supported-providers";

const USE_CASE_PRESETS = [
  "个人助理",
  "编码助手",
  "客服支持",
  "研究",
  "倾诉 / 日记",
];

const DEFAULT_TEST_MESSAGE = "我喜欢周末去徒步。";

export default function SetupPage() {
  const router = useRouter();
  const { register } = useAuth();
  const [step, setStep] = useState(0);
  const [isLoading, setIsLoading] = useState(false);
  const [isPrefillingConfig, setIsPrefillingConfig] = useState(false);
  const [error, setError] = useState("");

  const [name, setName] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [confirmPassword, setConfirmPassword] = useState("");

  const [llmProvider, setLlmProvider] = useState("");
  const [llmModel, setLlmModel] = useState("");
  const [llmApiKey, setLlmApiKey] = useState("");
  const [llmBaseUrl, setLlmBaseUrl] = useState("");
  const [embedderProvider, setEmbedderProvider] = useState("");
  const [embedderModel, setEmbedderModel] = useState("");
  const [embedderBaseUrl, setEmbedderBaseUrl] = useState("");
  const [embedderApiKey, setEmbedderApiKey] = useState("");
  const [embeddingDims, setEmbeddingDims] = useState("");
  const [serverHasLlmKey, setServerHasLlmKey] = useState(false);
  const [serverHasEmbedderKey, setServerHasEmbedderKey] = useState(false);
  const [providers, setProviders] = useState<BundledProviders | null>(null);
  const [initialLlmProvider, setInitialLlmProvider] = useState("");
  const [initialLlmModel, setInitialLlmModel] = useState("");
  const [initialLlmBaseUrl, setInitialLlmBaseUrl] = useState("");
  const [initialEmbedderProvider, setInitialEmbedderProvider] = useState("");
  const [initialEmbedderModel, setInitialEmbedderModel] = useState("");
  const [initialEmbedderBaseUrl, setInitialEmbedderBaseUrl] = useState("");
  const [initialEmbeddingDims, setInitialEmbeddingDims] = useState("");

  const [apiKey, setApiKey] = useState("");
  const [keyLabel, setKeyLabel] = useState("");
  const [copied, setCopied] = useState(false);
  const [testSuccess, setTestSuccess] = useState(false);

  const [useCase, setUseCase] = useState("");
  const [customInstructions, setCustomInstructions] = useState("");
  const [testMessage, setTestMessage] = useState(DEFAULT_TEST_MESSAGE);
  const [isGeneratingInstructions, setIsGeneratingInstructions] =
    useState(false);

  const apiBase = displayApiUrl();
  const setupStatus = useSetupStatus();
  // A local install has no accounts, so the admin-account step has nothing to collect.
  const localMode =
    setupStatus.state === "needs-setup" || setupStatus.state === "configured"
      ? setupStatus.localMode
      : false;

  useEffect(() => {
    // Already-onboarded installs have no business in the wizard; middleware used to bounce them
    // to /login before this page rendered. A local install has no login to bounce to.
    if (setupStatus.state !== "configured") return;
    router.replace(setupStatus.localMode ? "/dashboard/requests/" : "/login/");
  }, [setupStatus, router]);

  useEffect(() => {
    if (localMode && step === 0) setStep(1);
  }, [localMode, step]);

  useEffect(() => {
    if (step !== 1) {
      setIsPrefillingConfig(false);
      return;
    }

    let active = true;
    setIsPrefillingConfig(true);

    const loadConfig = async () => {
      try {
        const [configRes, providersRes] = await Promise.all([
          api.get(MEMORY_ENDPOINTS.CONFIGURE),
          api.get<BundledProviders>(MEMORY_ENDPOINTS.CONFIGURE_PROVIDERS),
        ]);
        const config = getEffectiveConfig(configRes.data);

        if (!active) {
          return;
        }

        const llmProv = config?.llm?.provider || "";
        const llmMod = config?.llm?.config?.model || "";
        const llmBase = readBaseUrl(config?.llm);
        const embProv = config?.embedder?.provider || "";
        const embMod = config?.embedder?.config?.model || "";
        const embBase = readBaseUrl(config?.embedder);
        // The vector column width constrains what can be stored, so it is the fallback when the
        // embedder section does not state its own dimensions.
        const dims =
          config?.embedder?.config?.embedding_dims ??
          config?.vector_store?.config?.embedding_model_dims;
        const dimsValue = dims ? String(dims) : "";

        setLlmProvider(llmProv);
        setLlmModel(llmMod);
        setLlmBaseUrl(llmBase);
        setEmbedderProvider(embProv);
        setEmbedderModel(embMod);
        setEmbedderBaseUrl(embBase);
        setEmbeddingDims(dimsValue);
        setInitialLlmProvider(llmProv);
        setInitialLlmModel(llmMod);
        setInitialLlmBaseUrl(llmBase);
        setInitialEmbedderProvider(embProv);
        setInitialEmbedderModel(embMod);
        setInitialEmbedderBaseUrl(embBase);
        setInitialEmbeddingDims(dimsValue);
        setServerHasLlmKey(!!config?.llm?.config?.api_key);
        // A redacted value means the server holds one, so the field can stay blank without erasing it.
        setServerHasEmbedderKey(!!config?.embedder?.config?.api_key);
        setProviders(providersRes.data);
      } catch (err) {
        if (active) {
          setError(getErrorMessage(err, "无法读取服务端配置"));
        }
      } finally {
        if (active) {
          setIsPrefillingConfig(false);
        }
      }
    };

    void loadConfig();

    return () => {
      active = false;
    };
  }, [step]);

  /**
   * A local install has no account and no login, so the steps after providers — an API key to
   * authenticate with, a use case, a test call — have nothing left to set up. Ask the server
   * whether the runtime is ready and go straight to the dashboard when it is, rather than walking
   * an operator through screens that only exist for a server with accounts.
   */
  const finishLocalOnboarding = async (): Promise<boolean> => {
    if (!localMode) return false;
    try {
      const res = await fetch(apiUrl(AUTH_ENDPOINTS.SETUP_STATUS));
      if (!res.ok) return false;
      const { needsSetup } = await res.json();
      if (needsSetup) return false;
      router.replace("/dashboard/requests/");
      return true;
    } catch {
      // Fall through: the next step will surface whatever is still missing.
      return false;
    }
  };

  const handleStep1 = async (e: React.FormEvent) => {
    e.preventDefault();

    if (!isValidEmail(email)) {
      setError("请输入有效的邮箱地址。");
      return;
    }

    if (password !== confirmPassword) {
      setError("两次输入的密码不一致");
      return;
    }

    if (password.length < 8) {
      setError("密码至少需要 8 个字符");
      return;
    }

    setError("");
    setIsLoading(true);

    try {
      await register(name, email, password);
      setStep(1);
    } catch (err) {
      setError(getErrorMessage(err, "注册失败"));
    } finally {
      setIsLoading(false);
    }
  };

  const handleStep2 = async (e: React.FormEvent) => {
    e.preventDefault();
    setError("");

    const dirty =
      !!llmApiKey ||
      !!embedderApiKey ||
      llmProvider !== initialLlmProvider ||
      llmModel !== initialLlmModel ||
      llmBaseUrl !== initialLlmBaseUrl ||
      embedderProvider !== initialEmbedderProvider ||
      embedderModel !== initialEmbedderModel ||
      embedderBaseUrl !== initialEmbedderBaseUrl ||
      embeddingDims !== initialEmbeddingDims;

    if (!dirty) {
      if (await finishLocalOnboarding()) return;
      setStep(2);
      return;
    }

    setIsLoading(true);
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

      const payload: Record<string, unknown> = { version: "v1.1" };
      if (llm) payload.llm = llm;
      if (embedder) payload.embedder = embedder;

      await api.post(MEMORY_ENDPOINTS.CONFIGURE, payload);
      if (llmApiKey) setServerHasLlmKey(true);
      if (embedderApiKey) setServerHasEmbedderKey(true);
      setLlmApiKey("");
      setEmbedderApiKey("");
      setInitialLlmProvider(llmProvider);
      setInitialLlmModel(llmModel);
      setInitialLlmBaseUrl(llmBaseUrl);
      setInitialEmbedderProvider(embedderProvider);
      setInitialEmbedderModel(embedderModel);
      setInitialEmbedderBaseUrl(embedderBaseUrl);
      setInitialEmbeddingDims(embeddingDims);
      if (await finishLocalOnboarding()) return;
      setStep(2);
    } catch (err) {
      setError(getErrorMessage(err, "保存配置失败"));
    } finally {
      setIsLoading(false);
    }
  };

  const handleStep3 = async (e: React.FormEvent) => {
    e.preventDefault();
    setError("");
    setIsLoading(true);

    try {
      const res = await api.post(API_KEY_ENDPOINTS.BASE, {
        label: keyLabel.trim() || "My First Key",
      });
      setApiKey(res.data.key);
    } catch (err) {
      setError(getErrorMessage(err, "创建 API 密钥失败"));
    } finally {
      setIsLoading(false);
    }
  };

  const handleContinueToUseCase = (e: React.FormEvent) => {
    e.preventDefault();
    setStep(3);
  };

  const handleContinueToQuickTest = () => {
    setError("");
    setStep(4);
  };

  const handleGoToDashboard = (e: React.FormEvent) => {
    e.preventDefault();
    router.push("/dashboard/requests");
  };

  const handleTest = async (e: React.FormEvent) => {
    e.preventDefault();
    setError("");
    setIsLoading(true);

    try {
      const res = await fetch(apiUrl("/memories"), {
        method: "POST",
        headers: { "Content-Type": "application/json", "X-API-Key": apiKey },
        body: JSON.stringify({
          messages: [{ role: "user", content: testMessage }],
          user_id: "setup-test",
        }),
      });

      if (!res.ok) {
        const body = await res.text();
        let detail = body;
        try {
          detail = JSON.parse(body).detail ?? body;
        } catch {}
        throw new Error(
          `Test failed (${res.status}): ${detail || res.statusText}`,
        );
      }

      setTestSuccess(true);
      void api
        .post(AUTH_ENDPOINTS.ONBOARDING_COMPLETE, { use_case: useCase })
        .catch(() => {});
    } catch (err) {
      setError(getErrorMessage(err, "测试失败"));
    } finally {
      setIsLoading(false);
    }
  };

  return (
    <div className="min-h-screen flex items-center justify-center bg-surface-default-primary p-4">
      <div className="w-full max-w-[560px] space-y-6">
        <div className="flex items-center justify-center gap-2">
          {STEPS.map((label, i) => {
            // Skipping the admin step renumbers the remaining ones, so the visible dots count from
            // one rather than starting at two.
            if (localMode && i === 0) return null;

            return (
              <div key={label} className="flex items-center gap-2">
                <div
                  className={cn(
                    "size-7 rounded-full flex items-center justify-center text-xs font-medium",
                    i <= step
                      ? "bg-memPurple-500 text-white"
                      : "bg-memNeutral-200 text-onSurface-default-tertiary",
                  )}
                >
                  {i < step ? (
                    <Check className="size-3.5" />
                  ) : localMode ? (
                    i
                  ) : (
                    i + 1
                  )}
                </div>
                {i < STEPS.length - 1 && (
                  <div
                    className={cn(
                      "w-8 h-[2px]",
                      i < step ? "bg-memPurple-500" : "bg-memNeutral-200",
                    )}
                  />
                )}
              </div>
            );
          })}
        </div>
        <p className="text-center text-sm text-onSurface-default-tertiary">
          {STEPS[step]}
        </p>

        <Card className="border-memBorder-primary">
          <CardContent className="p-6 space-y-4">
            <div className="space-y-1">
              <h2 className="text-base font-semibold font-fustat">
                {STEP_TITLES[step]}
              </h2>
              {isPrefillingConfig && step === 1 && (
                <p className="text-xs text-onSurface-default-tertiary">
                  正在读取服务端配置…
                </p>
              )}
            </div>
            {error && (
              <p className="text-sm text-onSurface-danger-primary">{error}</p>
            )}

            {step === 0 && !localMode && (
              <form onSubmit={handleStep1} className="space-y-4">
                <div className="space-y-1">
                  <Label htmlFor="setup-name">名称</Label>
                  <Input
                    id="setup-name"
                    value={name}
                    onChange={(e) => setName(e.target.value)}
                    placeholder="你的名字"
                  />
                </div>
                <div className="space-y-1">
                  <Label htmlFor="setup-email">邮箱</Label>
                  <Input
                    id="setup-email"
                    type="email"
                    value={email}
                    onChange={(e) => setEmail(e.target.value)}
                    placeholder="admin@company.com"
                  />
                </div>
                <div className="space-y-1">
                  <Label htmlFor="setup-password">密码</Label>
                  <Input
                    id="setup-password"
                    type="password"
                    value={password}
                    onChange={(e) => setPassword(e.target.value)}
                    placeholder="至少 8 个字符"
                  />
                </div>
                <div className="space-y-1">
                  <Label htmlFor="setup-confirm-password">确认密码</Label>
                  <Input
                    id="setup-confirm-password"
                    type="password"
                    value={confirmPassword}
                    onChange={(e) => setConfirmPassword(e.target.value)}
                  />
                </div>
                <Button
                  type="submit"
                  disabled={isLoading || !name || !email || !password}
                  className="w-full"
                >
                  {isLoading ? "正在创建…" : "创建管理员账号"}
                </Button>
              </form>
            )}

            {step === 1 && (
              <form onSubmit={handleStep2} className="space-y-4">
                {!serverHasLlmKey && (
                  <div className="rounded-md border border-memBorder-primary bg-surface-default-secondary p-3">
                    <p className="text-xs text-onSurface-default-tertiary">
                      服务端还没有配置 LLM 的 API
                      密钥。在下面填一个即可继续——它会保存到服务端，之后所有记忆操作都用它。
                    </p>
                  </div>
                )}

                <div className="grid grid-cols-2 gap-3">
                  <div className="space-y-1">
                    <Label htmlFor="setup-llm-provider">LLM Provider</Label>
                    <Select
                      value={llmProvider}
                      onValueChange={(value) => {
                        // Radix emits "" while the option list is still empty, because it cannot
                        // resolve the value the server just sent. Taking that literally would
                        // erase the prefilled provider and leave the wizard unable to continue.
                        if (!value) return;
                        setLlmProvider(value);
                        setLlmApiKey("");
                      }}
                      disabled={!providers}
                    >
                      <SelectTrigger id="setup-llm-provider">
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
                    <Label htmlFor="setup-llm-model">模型</Label>
                    <Input
                      id="setup-llm-model"
                      value={llmModel}
                      onChange={(e) => setLlmModel(e.target.value)}
                      placeholder={
                        EXAMPLE_MODEL[llmProvider] ?? "gpt-4.1-nano-2025-04-14"
                      }
                      className="font-mono text-sm"
                    />
                  </div>
                </div>

                <div className="space-y-1">
                  <Label htmlFor="setup-llm-api-key">LLM API 密钥</Label>
                  <Input
                    id="setup-llm-api-key"
                    type="password"
                    value={llmApiKey}
                    onChange={(e) => setLlmApiKey(e.target.value)}
                    placeholder={
                      serverHasLlmKey ? "留空表示沿用已有密钥" : "sk-..."
                    }
                    className="font-mono text-sm"
                  />
                  <p className="text-xs text-onSurface-default-tertiary">
                    供 LLM 使用。嵌入模型有自己的密钥，在下面单独填。
                  </p>
                </div>

                <div className="space-y-1">
                  <Label htmlFor="setup-llm-base-url">LLM Base URL</Label>
                  <Input
                    id="setup-llm-base-url"
                    value={llmBaseUrl}
                    onChange={(e) => setLlmBaseUrl(e.target.value)}
                    placeholder="留空则用该 provider 的默认值"
                    className="font-mono text-sm"
                  />
                  <p className="text-xs text-onSurface-default-tertiary">
                    需要走代理、网关或自建端点时填这里。
                  </p>
                </div>

                <div className="grid grid-cols-2 gap-3">
                  <div className="space-y-1">
                    <Label htmlFor="setup-embedder-provider">
                      嵌入 Provider
                    </Label>
                    <Select
                      value={embedderProvider}
                      onValueChange={(value) => {
                        // Same as the LLM section: Radix emits "" before the options exist.
                        if (!value) return;
                        setEmbedderProvider(value);
                        // A key belongs to a provider, so switching one must not carry the other's
                        // credential along.
                        setEmbedderApiKey("");
                        // The stored key belongs to the previous provider, so it no longer counts
                        // as one this section can fall back on.
                        setServerHasEmbedderKey(false);
                        // The vector column is fixed-width: keeping the old width after a provider
                        // switch produces vectors the table cannot hold.
                        const next = DEFAULT_EMBEDDING_DIMS[value];
                        if (next) setEmbeddingDims(String(next));
                      }}
                      disabled={!providers}
                    >
                      <SelectTrigger id="setup-embedder-provider">
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
                    <Label htmlFor="setup-embedder-model">模型</Label>
                    <Input
                      id="setup-embedder-model"
                      value={embedderModel}
                      onChange={(e) => setEmbedderModel(e.target.value)}
                      placeholder={
                        EXAMPLE_EMBEDDER_MODEL[embedderProvider] ??
                        "text-embedding-3-small"
                      }
                      className="font-mono text-sm"
                    />
                  </div>
                </div>

                <div className="space-y-1">
                  <Label htmlFor="setup-embedder-api-key">
                    嵌入模型 API 密钥
                  </Label>
                  <Input
                    id="setup-embedder-api-key"
                    type="password"
                    value={embedderApiKey}
                    onChange={(e) => setEmbedderApiKey(e.target.value)}
                    placeholder={
                      serverHasEmbedderKey
                        ? "留空表示沿用已保存的密钥"
                        : "sk-..."
                    }
                    className="font-mono text-sm"
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
                    <Label htmlFor="setup-embedder-dims">维度</Label>
                    <Input
                      id="setup-embedder-dims"
                      type="number"
                      inputMode="numeric"
                      value={embeddingDims}
                      onChange={(e) => setEmbeddingDims(e.target.value)}
                      placeholder="1536"
                      className="font-mono text-sm"
                    />
                  </div>
                  <div className="space-y-1">
                    <Label htmlFor="setup-embedder-base-url">
                      嵌入 Base URL
                    </Label>
                    <Input
                      id="setup-embedder-base-url"
                      value={embedderBaseUrl}
                      onChange={(e) => setEmbedderBaseUrl(e.target.value)}
                      placeholder="默认"
                      className="font-mono text-sm"
                    />
                  </div>
                </div>
                <p className="text-xs text-onSurface-default-tertiary">
                  维度必须与模型一致。它决定了向量列的宽度，之后要改必须先清空记忆。
                </p>

                <p className="text-xs text-onSurface-default-tertiary">
                  需要别的
                  provider？把它的依赖装进来、重新构建，并加进内置列表。参见{" "}
                  <a
                    href={SUPPORTED_PROVIDERS_URL}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="underline underline-offset-4 hover:text-onSurface-default-primary"
                  >
                    支持的 provider
                  </a>
                  .
                </p>

                <Button
                  type="submit"
                  disabled={
                    isLoading ||
                    !llmProvider ||
                    !embedderProvider ||
                    (!serverHasLlmKey && !llmApiKey)
                  }
                  className="w-full"
                >
                  {isLoading ? "正在保存…" : "保存并继续"}
                </Button>
              </form>
            )}

            {step === 2 && !apiKey && (
              <form onSubmit={handleStep3} className="space-y-4">
                <div className="space-y-1">
                  <Label htmlFor="setup-key-label">给这枚密钥起个备注</Label>
                  <Input
                    id="setup-key-label"
                    value={keyLabel}
                    onChange={(e) => setKeyLabel(e.target.value)}
                    placeholder="我的第一枚密钥"
                  />
                </div>
                <Button type="submit" disabled={isLoading} className="w-full">
                  {isLoading ? "正在生成…" : "生成 API 密钥"}
                </Button>
              </form>
            )}

            {step === 2 && apiKey && (
              <form onSubmit={handleContinueToUseCase} className="space-y-4">
                <div className="space-y-1">
                  <Label htmlFor="setup-api-key">你的 API 密钥</Label>
                  <div className="flex gap-2">
                    <Input
                      id="setup-api-key"
                      value={apiKey}
                      readOnly
                      className="font-mono text-sm"
                    />
                    <CopyToClipboard
                      text={apiKey}
                      onCopy={() => {
                        setCopied(true);
                        setTimeout(() => setCopied(false), 2000);
                      }}
                    >
                      <Button variant="outline" size="icon">
                        {copied ? (
                          <Check className="size-4" />
                        ) : (
                          <Copy className="size-4" />
                        )}
                      </Button>
                    </CopyToClipboard>
                  </div>
                  <p className="text-xs text-onSurface-danger-primary">
                    请立即保存这枚密钥，关闭后将无法再次查看。
                  </p>
                </div>
                <Button type="submit" className="w-full">
                  继续
                </Button>
              </form>
            )}

            {step === 3 && (
              <div className="space-y-4">
                <div className="space-y-1">
                  <Label htmlFor="setup-use-case">描述你的使用场景</Label>
                  <textarea
                    id="setup-use-case"
                    value={useCase}
                    onChange={(e) => setUseCase(e.target.value)}
                    placeholder="例如：一个记得我偏好的个人助理"
                    className="flex w-full rounded-md border border-memBorder-primary bg-surface-default-primary px-3 py-2 text-sm placeholder:text-onSurface-default-tertiary focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-memPurple-500 min-h-[80px] resize-y"
                  />
                </div>
                <div className="flex flex-wrap gap-2">
                  {USE_CASE_PRESETS.map((preset) => (
                    <Button
                      key={preset}
                      type="button"
                      variant="outline"
                      size="sm"
                      onClick={() => setUseCase(preset)}
                      className={cn(
                        useCase === preset &&
                          "border-memPurple-500 text-memPurple-500",
                      )}
                    >
                      {preset}
                    </Button>
                  ))}
                </div>
                <p className="text-xs text-onSurface-default-tertiary">
                  我们会据此生成一段自定义指令，告诉记忆抽取器在这个场景下优先记住哪些事实。
                </p>
                <div className="flex gap-2">
                  <Button
                    type="button"
                    variant="outline"
                    onClick={handleContinueToQuickTest}
                    className="flex-1"
                  >
                    跳过
                  </Button>
                  <Button
                    type="button"
                    disabled={!useCase || isGeneratingInstructions}
                    className="flex-1"
                    onClick={async () => {
                      setError("");
                      setIsGeneratingInstructions(true);
                      try {
                        const res = await api.post(
                          MEMORY_ENDPOINTS.GENERATE_INSTRUCTIONS,
                          {
                            use_case: useCase,
                          },
                        );
                        setCustomInstructions(res.data.custom_instructions);
                        if (res.data.test_message) {
                          setTestMessage(res.data.test_message);
                        }
                      } catch (err) {
                        setError(getErrorMessage(err, "生成指令失败"));
                      } finally {
                        setIsGeneratingInstructions(false);
                      }
                    }}
                  >
                    {isGeneratingInstructions ? "正在生成指令…" : "生成指令"}
                  </Button>
                </div>
                {customInstructions && (
                  <div className="space-y-3">
                    <div className="space-y-1">
                      <Label htmlFor="setup-instructions">生成的指令</Label>
                      <textarea
                        id="setup-instructions"
                        value={customInstructions}
                        onChange={(e) => setCustomInstructions(e.target.value)}
                        className="flex w-full rounded-md border border-memBorder-primary bg-surface-default-primary px-3 py-2 text-sm placeholder:text-onSurface-default-tertiary focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-memPurple-500 min-h-[120px] resize-y"
                      />
                    </div>
                    <Button
                      type="button"
                      className="w-full"
                      onClick={async () => {
                        setError("");
                        setIsLoading(true);
                        try {
                          await api.post(MEMORY_ENDPOINTS.CONFIGURE, {
                            custom_instructions: customInstructions,
                          });
                          handleContinueToQuickTest();
                        } catch (err) {
                          setError(getErrorMessage(err, "保存指令失败"));
                        } finally {
                          setIsLoading(false);
                        }
                      }}
                      disabled={isLoading}
                    >
                      {isLoading ? "正在保存…" : "保存并继续"}
                    </Button>
                  </div>
                )}
              </div>
            )}

            {step === 4 && (
              <form
                onSubmit={testSuccess ? handleGoToDashboard : handleTest}
                className="space-y-4"
              >
                <div className="space-y-1">
                  <Label>验证你的配置</Label>
                  {!apiBase && (
                    <p className="text-xs text-onSurface-danger-primary">
                      NEXT_PUBLIC_API_URL 没有设置，页面来源也不可用。请先在
                      .env 里配置并重启，再运行这个测试。
                    </p>
                  )}
                  <pre className="text-xs bg-surface-default-secondary p-3 rounded font-mono overflow-x-auto">{`curl -X POST ${apiBase}/memories \\
  -H "X-API-Key: ${apiKey}" \\
  -H "Content-Type: application/json" \\
  -d '{"messages": [{"role": "user", "content": "${testMessage}"}], "user_id": "setup-test"}'`}</pre>
                </div>
                {!testSuccess ? (
                  <>
                    <Button
                      type="submit"
                      disabled={isLoading}
                      className="w-full"
                    >
                      {isLoading ? "正在测试…" : "运行测试"}
                    </Button>
                    {error && (
                      <p className="text-xs text-onSurface-default-tertiary">
                        provider 的凭证或模型填错了？去{" "}
                        <a
                          href="/dashboard/configuration"
                          className="underline underline-offset-4 hover:text-onSurface-default-primary"
                        >
                          「配置」
                        </a>{" "}
                        改好后重新运行测试。
                      </p>
                    )}
                  </>
                ) : (
                  <div className="space-y-3">
                    <div className="flex items-center gap-2 text-sm text-onSurface-positive-primary">
                      <Check className="size-4" /> 记忆写入成功
                    </div>
                    <Button type="submit" className="w-full">
                      进入控制台
                    </Button>
                  </div>
                )}
              </form>
            )}
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
