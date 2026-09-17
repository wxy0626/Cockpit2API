/**
 * CindyGatewayPanel —— Cindy 反代网关面板。
 *
 * 数据来自 sidecars/cindy2api（Go sidecar，默认监听 127.0.0.1:7865）：
 *   GET  /api/config              接入信息（Base URL / 本地 API Key）
 *   GET  /api/status              账号池状态
 *   GET  /api/models              模型清单
 *   POST /api/check               重新检测账号
 *   POST /v1/chat/completions     测试发送（需带本地 API Key）
 *
 * 账号由 sidecar 自动从本机 Cindy 桌面端数据目录发现，这里只做展示与触发，
 * 不读取任何凭据 —— 上游 key 永不进入前端。
 *
 * 样式复用 workbuddy.css 里已有的网关面板类（wb-*），保持与 WorkBuddy 页视觉一致，
 * 不新增共享样式文件，避免与上游合并冲突。
 */
import { useCallback, useEffect, useState } from 'react';
import { Check, Copy, Eye, EyeOff, Play, Plus, RefreshCw } from 'lucide-react';

import { AddCindyAccountDialog } from './AddCindyAccountDialog';

/** sidecar 的默认监听地址，与 sidecars/cindy2api/runtime/config.json 的 listen 对应 */
const CINDY_BASE = 'http://127.0.0.1:7865';

interface CindyConfig {
  /** sidecar 的完整配置（含自动生成的本地 api_key） */
  config?: { listen?: string; api_key?: string; [key: string]: unknown };
  baseUrl?: string;
  lan_base_url?: string | null;
}

interface CindyAccount {
  ownerId: string;
  keyMasked: string;
  endpoint: string;
  /** 账号来源：local = 本机 Cindy 登录态；oauth = 本工具授权添加 */
  source?: string;
  status: string;
  statusDetail: string;
  modelCount: number;
  latencyMs: number;
}

interface CindyStatus {
  total?: number;
  healthy?: number;
  accounts?: CindyAccount[];
}

export function CindyGatewayPanel() {
  const [config, setConfig] = useState<CindyConfig | null>(null);
  const [status, setStatus] = useState<CindyStatus | null>(null);
  const [models, setModels] = useState<string[]>([]);
  const [showKey, setShowKey] = useState(false);
  const [copied, setCopied] = useState('');
  const [message, setMessage] = useState('你好，请用一句话自我介绍。');
  const [model, setModel] = useState('');
  const [result, setResult] = useState('');
  const [loading, setLoading] = useState(false);
  const [checking, setChecking] = useState(false);
  const [dialogOpen, setDialogOpen] = useState(false);

  const apiKey = config?.config?.api_key ?? '';
  const base = config?.baseUrl ?? `${CINDY_BASE}/v1`;
  const lan = config?.lan_base_url || '';
  const dockerBase = 'http://host.docker.internal:7865/v1';
  const available = status?.healthy ?? 0;
  const total = status?.total ?? 0;
  const connected = config !== null;

  /** 拉取接入信息、账号状态与模型清单 */
  const load = useCallback(async () => {
    setLoading(true);
    try {
      const [configData, statusData, modelsData] = await Promise.all([
        fetch(`${CINDY_BASE}/api/config`).then((r) => r.json()),
        fetch(`${CINDY_BASE}/api/status`).then((r) => r.json()),
        fetch(`${CINDY_BASE}/api/models`).then((r) => r.json()),
      ]);
      setConfig(configData as CindyConfig);
      setStatus(statusData as CindyStatus);
      const list = Array.isArray((modelsData as { data?: Array<{ id: string }> }).data)
        ? (modelsData as { data: Array<{ id: string }> }).data.map((item) => item.id)
        : [];
      setModels(list);
      setModel((current) => current || list[0] || '');
      setResult('');
    } catch (error) {
      const detail = error instanceof Error ? error.message : String(error);
      setConfig(null);
      setResult(
        `无法连接 Cindy 网关（${CINDY_BASE}）：${detail}\n` +
          '请先启动 sidecar：sidecars/cindy2api/bin/cindy2api.exe',
      );
    } finally {
      setLoading(false);
    }
  }, []);

  /** 触发 sidecar 重新检测所有账号 */
  const checkAccounts = useCallback(async () => {
    setChecking(true);
    try {
      const data = (await fetch(`${CINDY_BASE}/api/check`, { method: 'POST' }).then((r) =>
        r.json(),
      )) as CindyStatus;
      setStatus(data);
    } catch (error) {
      setResult(`检测失败：${error instanceof Error ? error.message : String(error)}`);
    } finally {
      setChecking(false);
    }
  }, []);

  /** 通过网关自身发一次真实请求，验证端到端可用 */
  const test = useCallback(async () => {
    if (!apiKey) {
      setResult('尚未取得网关 API Key，请先点右上角刷新。');
      return;
    }
    setResult('测试中…');
    const started = Date.now();
    try {
      const response = await fetch(`${CINDY_BASE}/v1/chat/completions`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json', Authorization: `Bearer ${apiKey}` },
        body: JSON.stringify({
          model: model || 'deepseek/deepseek-v4-flash',
          messages: [{ role: 'user', content: message }],
          max_tokens: 128,
        }),
      });
      const text = await response.text();
      const account = response.headers.get('x-gateway-account') ?? '-';
      const upstream = response.headers.get('x-gateway-upstream') ?? '-';
      if (!response.ok) {
        setResult(`HTTP ${response.status} · 账号 ${account}\n${text.slice(0, 600)}`);
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
          `HTTP ${response.status} · ${Date.now() - started} ms · 账号 ${account} · 上游 ${upstream}`,
          parsed.usage?.total_tokens ? `用量：${parsed.usage.total_tokens} tokens` : '',
          reasoning ? `\n[思考]\n${reasoning}` : '',
          content ? `\n[回复]\n${content}` : '',
        ]
          .filter(Boolean)
          .join('\n'),
      );
    } catch (error) {
      setResult(`调用失败：${error instanceof Error ? error.message : String(error)}`);
    }
  }, [apiKey, message, model]);

  /** 复制并短暂提示 */
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

  return (
    <section className="workbuddy-api-gateway-panel">
      <header>
        <h2>
          Cindy 反代网关 <span className="wb-status">{connected ? '已连接' : '未连接'}</span>
        </h2>
        <div style={{ display: 'flex', gap: 8 }}>
          <button onClick={() => setDialogOpen(true)} title="添加账号（OAuth 授权或本机导入）">
            <Plus size={16} /> 添加账号
          </button>
          <button onClick={() => void load()} title="刷新">
            <RefreshCw className={loading ? 'spin' : ''} size={16} /> 刷新
          </button>
        </div>
      </header>

      <AddCindyAccountDialog
        open={dialogOpen}
        onClose={() => setDialogOpen(false)}
        onAdded={() => void load()}
      />

      <div className="wb-gateway-card">
        <div className="wb-row">
          <strong>Base URL</strong>
          <code>{base}</code>
          <button onClick={() => void copy('base', base)}>
            {copied === 'base' ? <Check size={15} /> : <Copy size={15} />}复制
          </button>
        </div>

        {lan && (
          <div className="wb-row">
            <strong>局域网访问</strong>
            <code>{lan}</code>
            <button onClick={() => void copy('lan', lan)}>
              {copied === 'lan' ? <Check size={15} /> : <Copy size={15} />}复制
            </button>
          </div>
        )}

        <div className="wb-row">
          <strong>容器访问</strong>
          <code>{dockerBase}</code>
          <span title="Docker 容器内运行的服务（如 sub2api）使用此地址访问宿主机网关">ⓘ</span>
          <button onClick={() => void copy('docker', dockerBase)}>
            {copied === 'docker' ? <Check size={15} /> : <Copy size={15} />}复制
          </button>
        </div>

        <div className="wb-row">
          <strong>API Key</strong>
          <code>{apiKey ? (showKey ? apiKey : '•'.repeat(Math.min(apiKey.length, 32))) : '未配置'}</code>
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
          <b title="账号由 sidecar 自动从本机 Cindy 登录数据发现；endpoint 与 key 同租户成对使用">
            {loading && total === 0 ? '读取中' : `${available} / ${total} 可用`}
          </b>
          <button onClick={() => void checkAccounts()} disabled={checking}>
            <RefreshCw className={checking ? 'spin' : ''} size={14} />
            {checking ? '检测中…' : '重新检测'}
          </button>
        </div>

        {Array.isArray(status?.accounts) && status.accounts.length > 0 && (
          <div className="wb-row" style={{ display: 'block' }}>
            <table style={{ width: '100%', borderCollapse: 'collapse', fontSize: 13 }}>
              <thead>
                <tr style={{ color: 'var(--text-secondary)', textAlign: 'left' }}>
                  <th style={{ padding: '6px 8px' }}>网关入口</th>
                  <th style={{ padding: '6px 8px' }}>来源</th>
                  <th style={{ padding: '6px 8px' }}>凭据</th>
                  <th style={{ padding: '6px 8px' }}>模型</th>
                  <th style={{ padding: '6px 8px' }}>延迟</th>
                  <th style={{ padding: '6px 8px' }}>状态</th>
                </tr>
              </thead>
              <tbody>
                {status.accounts.map((account) => (
                  <tr key={account.ownerId} style={{ borderTop: '1px solid var(--border-subtle)' }}>
                    <td style={{ padding: '6px 8px' }}>{account.endpoint.replace('https://', '')}</td>
                    <td style={{ padding: '6px 8px', color: 'var(--text-secondary)' }}>
                      {account.source === 'oauth' ? '授权添加' : '本机登录态'}
                    </td>
                    <td style={{ padding: '6px 8px', color: 'var(--text-secondary)' }}>
                      {account.keyMasked}
                    </td>
                    <td style={{ padding: '6px 8px' }}>{account.modelCount} 个</td>
                    <td style={{ padding: '6px 8px' }}>{account.latencyMs ? `${account.latencyMs} ms` : '-'}</td>
                    <td
                      style={{
                        padding: '6px 8px',
                        color: account.status === 'ok' ? '#56e0b0' : 'var(--text-secondary)',
                      }}
                    >
                      {account.status === 'ok' ? '可用' : account.statusDetail}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}

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
            {models.map((id) => (
              <option key={id} value={id}>
                {id}
              </option>
            ))}
          </select>
          <input value={message} onChange={(event) => setMessage(event.target.value)} />
          <button onClick={() => void test()}>
            <Play size={15} />发送
          </button>
        </div>

        {result && <pre className="wb-result">{result}</pre>}
      </div>
    </section>
  );
}
