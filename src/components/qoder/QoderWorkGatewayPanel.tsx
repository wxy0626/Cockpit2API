/**
 * QoderWorkGatewayPanel —— Qoder 统一反代网关面板。
 *
 * 账号来源是本机 QoderWork 登录态；网关刷新 token 后提供 OpenAI Chat Completions
 * 兼容接口。前端只拿本地网关 Key 和脱敏状态，不接触 Qoder 原始凭据。
 */
import { useCallback, useEffect, useState } from 'react';
import { Check, Copy, Eye, EyeOff, Play, RefreshCw } from 'lucide-react';

const GATEWAY_BASE = 'http://127.0.0.1:7866';

/** 模型 ID -> 上游实际名称；后端清单不可用时仍保留基本可读性。 */
const MODEL_DISPLAY_NAMES: Record<string, string> = {
  'qwork-ultimate': 'Premium',
  'qwork-advanced': 'Advanced',
  'qwork-auto': 'Standard',
  smodel: 'Sonus',
  qmodel_38max: 'Qwen3.8-Max',
  qfmodel: 'Qwen3.8-Flash',
  qmodel_latest: 'Qwen3.7-Max',
  qmodel: 'Qwen3.7-Plus',
};

interface QoderWorkConfig {
  api_key?: string;
  base_url?: string;
  docker_base_url?: string | null;
}

interface QoderWorkAccount {
  name?: string;
  email?: string;
  tier?: string;
}

interface QoderWorkStatus {
  available?: boolean;
  total?: number;
  healthy?: number;
  domestic_total?: number;
  international_total?: number;
  account?: QoderWorkAccount;
  detail?: string;
}

export function QoderWorkGatewayPanel() {
  const [config, setConfig] = useState<QoderWorkConfig | null>(null);
  const [status, setStatus] = useState<QoderWorkStatus | null>(null);
  const [models, setModels] = useState<string[]>([]);
  const [showKey, setShowKey] = useState(false);
  const [copied, setCopied] = useState('');
  const [message, setMessage] = useState('你好，请用一句话自我介绍。');
  const [model, setModel] = useState('');
  const [result, setResult] = useState('');
  const [loading, setLoading] = useState(false);
  const [testing, setTesting] = useState(false);

  const apiKey = config?.api_key ?? '';
  const baseUrl = config?.base_url || `${GATEWAY_BASE}/v1`;
  const dockerBaseUrl = config?.docker_base_url || '';
  const available = status?.healthy ?? 0;
  const total = status?.total ?? 0;
  const connected = config !== null;

  /** 拉取本地网关配置、状态与模型清单。 */
  const load = useCallback(async () => {
    setLoading(true);
    try {
      const [configData, statusData] = await Promise.all([
        fetch(`${GATEWAY_BASE}/api/config`).then((response) => response.json()),
        fetch(`${GATEWAY_BASE}/api/status`).then((response) => response.json()),
      ]);
      const nextConfig = configData as QoderWorkConfig;
      setConfig(nextConfig);
      setStatus(statusData as QoderWorkStatus);
      const modelsData = nextConfig.api_key
        ? await fetch(`${GATEWAY_BASE}/v1/models`, {
            headers: { Authorization: `Bearer ${nextConfig.api_key}` },
          }).then((response) => response.json())
        : { data: [] };
      const list = Array.isArray((modelsData as { data?: Array<{ id?: string }> }).data)
        ? (modelsData as { data: Array<{ id?: string }> }).data
            .map((item) => item.id || '')
            .filter(Boolean)
        : [];
      setModels(list);
      setModel((current) => current || list[0] || '');
      setResult('');
    } catch (error) {
      setConfig(null);
      setResult(
        `无法连接 QoderWork 网关（${GATEWAY_BASE}）：${
          error instanceof Error ? error.message : String(error)
        }`,
      );
    } finally {
      setLoading(false);
    }
  }, []);

  /** 走网关发一次非流式请求，完整验证登录态、token 刷新和上游调用。 */
  const test = useCallback(async () => {
    if (!apiKey) {
      setResult('尚未取得网关 API Key，请刷新后重试。');
      return;
    }
    setTesting(true);
    setResult('测试中…');
    const started = Date.now();
    try {
      const response = await fetch(`${GATEWAY_BASE}/v1/chat/completions`, {
        method: 'POST',
        headers: {
          Authorization: `Bearer ${apiKey}`,
          'Content-Type': 'application/json',
        },
        body: JSON.stringify({
          model: model || 'qmodel_38max',
          messages: [{ role: 'user', content: message }],
          stream: false,
        }),
      });
      const text = await response.text();
      if (!response.ok) {
        setResult(`HTTP ${response.status}\n${text.slice(0, 800)}`);
        return;
      }
      const parsed = JSON.parse(text) as {
        choices?: Array<{ message?: { content?: string; reasoning_content?: string } }>;
        usage?: { total_tokens?: number };
      };
      const content = parsed.choices?.[0]?.message?.content ?? '';
      const reasoning = parsed.choices?.[0]?.message?.reasoning_content ?? '';
      setResult(
        [
          `HTTP ${response.status} · ${Date.now() - started} ms`,
          parsed.usage?.total_tokens ? `用量：${parsed.usage.total_tokens} tokens` : '',
          reasoning ? `\n[思考]\n${reasoning}` : '',
          content ? `\n[回复]\n${content}` : '',
        ]
          .filter(Boolean)
          .join('\n'),
      );
    } catch (error) {
      setResult(`调用失败：${error instanceof Error ? error.message : String(error)}`);
    } finally {
      setTesting(false);
    }
  }, [apiKey, message, model]);

  const refreshStatus = useCallback(async () => {
    try {
      const response = await fetch(`${GATEWAY_BASE}/api/status`);
      if (response.ok) {
        setStatus((await response.json()) as QoderWorkStatus);
      }
    } catch {
      // 状态刷新失败时保留上一次结果，网关测试仍可单独给出明确错误。
    }
  }, []);

  const copy = useCallback(async (tag: string, value: string) => {
    try {
      await navigator.clipboard.writeText(value);
      setCopied(tag);
      window.setTimeout(() => setCopied(''), 1200);
    } catch {
      setResult('复制失败：浏览器拒绝了剪贴板访问');
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const handleFocus = () => {
      void refreshStatus();
    };
    const timer = window.setInterval(() => {
      void refreshStatus();
    }, 5000);
    window.addEventListener('focus', handleFocus);
    return () => {
      window.clearInterval(timer);
      window.removeEventListener('focus', handleFocus);
    };
  }, [refreshStatus]);

  const account = status?.account;

  return (
    <section className="workbuddy-api-gateway-panel">
      <header>
        <h2>
          Qoder API 网关 <span className="wb-status">{connected ? '已连接' : '未连接'}</span>
        </h2>
        <button onClick={() => void load()} title="刷新">
          <RefreshCw className={loading ? 'spin' : ''} size={16} />
        </button>
      </header>

      <div className="wb-gateway-card">
        <div className="wb-row">
          <strong>Base URL</strong>
          <code>{baseUrl}</code>
          <button onClick={() => void copy('base', baseUrl)}>
            {copied === 'base' ? <Check size={15} /> : <Copy size={15} />}复制
          </button>
        </div>

        {dockerBaseUrl && (
          <div className="wb-row">
            <strong>容器访问</strong>
            <code>{dockerBaseUrl}</code>
            <span title="Docker 容器内运行的服务（如 sub2api）使用此地址访问宿主机网关">ⓘ</span>
            <button onClick={() => void copy('docker', dockerBaseUrl)}>
              {copied === 'docker' ? <Check size={15} /> : <Copy size={15} />}复制
            </button>
          </div>
        )}

        <div className="wb-row">
          <strong>API Key</strong>
          <code>
            {apiKey ? (showKey ? apiKey : '•'.repeat(Math.min(apiKey.length, 32))) : '未配置'}
          </code>
          <button onClick={() => setShowKey((value) => !value)}>
            {showKey ? <EyeOff size={15} /> : <Eye size={15} />}
          </button>
          {apiKey && (
            <button onClick={() => void copy('key', apiKey)}>
              {copied === 'key' ? <Check size={15} /> : <Copy size={15} />}复制
            </button>
          )}
        </div>

        <div className="wb-meta">
          <span>账号状态</span>
          <b
            title={
              status?.detail ||
              `统一账号池：国内 ${status?.domestic_total ?? 0} 个，国际 ${status?.international_total ?? 0} 个`
            }
          >
            {loading && total === 0
              ? '读取中'
              : `${available} / ${total} 可用${
                  account?.email || account?.name ? ` · ${account.email || account.name}` : ''
                }`}
          </b>
        </div>

        <div className="wb-chat">
          <select
            value={model}
            onChange={(event) => setModel(event.target.value)}
            style={{
              background: 'var(--surface-tertiary)',
              border: '1px solid var(--border-subtle)',
              color: 'var(--text-primary)',
              borderRadius: 9,
              padding: '11px',
              minWidth: 200,
            }}
          >
            {(models.length > 0 ? models : ['qmodel_38max', 'qfmodel']).map((item) => (
              <option key={item} value={item}>
                {MODEL_DISPLAY_NAMES[item] || item}
              </option>
            ))}
          </select>
          <input value={message} onChange={(event) => setMessage(event.target.value)} />
          <button onClick={() => void test()} disabled={testing}>
            <Play size={15} />
            {testing ? '发送中' : '发送'}
          </button>
        </div>

        {result && <pre className="wb-result">{result}</pre>}
      </div>
    </section>
  );
}
