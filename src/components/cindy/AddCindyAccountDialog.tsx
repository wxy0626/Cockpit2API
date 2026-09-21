/**
 * AddCindyAccountDialog —— 「添加 Cindy 账号」对话框。
 *
 * 两个页签，对齐 WorkBuddy 那个对话框的形态：
 *   1. OAuth 授权：选区域与登录方式 → 在**可信授权窗口**打开授权页 → 轮询授权结果 →
 *      sidecar 自动完成 PKCE 兑换并取回 {endpoint, apiKey}
 *   2. 本机导入：一键扫描本机 Cindy 桌面端登录态（零交互，不需要授权）
 *
 * 全部凭据处理都在 sidecar 进程内完成，前端只拿到脱敏后的账号状态。
 *
 * 授权页统一走**可信授权窗口**，复用本机登录态和设备信任状态。
 * 复用就会「再次授权还是上一个账号」。因此请求 sidecar 时传 `openBrowser: false`
 * （默认它自己会拉起系统浏览器），改由本组件打开可信授权窗口。
 */
import { useCallback, useEffect, useRef, useState } from 'react';
import { Check, Copy, ExternalLink, Globe, Loader, MonitorSmartphone, Smartphone, X } from 'lucide-react';

import { closeOAuthWindow, openOAuthWindow } from '../../services/cindyService';
import { CindyEmailLoginForm } from './CindyEmailLoginForm';
import { CindyPhoneLoginForm } from './CindyPhoneLoginForm';

/** sidecar 的默认监听地址，与 sidecars/cindy2api/runtime/config.json 的 listen 对应 */
const CINDY_BASE = 'http://127.0.0.1:7865';

interface ProvidersResponse {
  region?: string;
  email?: boolean;
  phone?: boolean;
  social?: string[];
  /** 该区域是否支持桌面端浏览器授权 */
  desktopAuthorizationSupported?: boolean;
  /** 不支持时的原因说明（由 sidecar 按产品口径给出） */
  desktopAuthorizationHint?: string;
  /** 该区域是否支持手机号 + 短信验证码登录（中国大陆版为 true） */
  phoneCodeLoginSupported?: boolean;
  /** 人机验证信息（国际版邮箱验证码发送强制 Turnstile） */
  captcha?: {
    siteKey?: string;
    requiredFor?: string[];
  };
}

interface Props {
  open: boolean;
  onClose: () => void;
  /** 账号成功加入后回调（父组件据此刷新列表） */
  onAdded: () => void;
}

/** 登录方式的中文名 */
const PROVIDER_LABELS: Record<string, string> = {
  google: 'Google',
  apple: 'Apple',
};

/**
 * 可选的授权方式，固定按 **Google → Apple → 企业 SSO** 排列。
 *
 * 刻意不跟随服务端返回的顺序：实测 `providers` 给的是 `["apple","google"]`，
 * 跟着它排会让 Apple 占首位、默认也落在 Apple 上，与预期不符。
 * Google 是这里最常用的方式，位置固定更符合直觉。
 */
function authOptions(social: string[]): Array<{ key: string; label: string }> {
  const preferredOrder = ['google', 'apple'];
  const ordered = [
    ...preferredOrder.filter((id) => social.includes(id)),
    ...social.filter((id) => !preferredOrder.includes(id)),
  ];
  return [
    ...ordered.map((id) => ({ key: id, label: PROVIDER_LABELS[id] ?? id })),
    { key: 'sso', label: '企业 SSO' },
  ];
}

export function AddCindyAccountDialog({ open, onClose, onAdded }: Props) {
  // 三种添加方式并列成三个页签：国内手机号登录 / 国际账号登录 / 本机导入
  const [tab, setTab] = useState<'phone' | 'global' | 'local'>('phone');
  // 国际登录页签内的两种方式：邮箱验证码 / OAuth 授权（Google/Apple/企业 SSO）
  const [globalMode, setGlobalMode] = useState<'email' | 'oauth'>('email');
  // 国际页签只用于国际版账号，区域固定 global（国内走手机号页签）
  const region = 'global' as const;
  const [providers, setProviders] = useState<ProvidersResponse | null>(null);
  const [provider, setProvider] = useState<string>('google');
  /** 企业 SSO 的组织标识（选中 SSO 时填写） */
  const [ssoOrg, setSsoOrg] = useState('');
  const [authorizeUrl, setAuthorizeUrl] = useState('');
  const [phase, setPhase] = useState<'idle' | 'waiting' | 'done' | 'error'>('idle');
  const [message, setMessage] = useState('');
  const [importing, setImporting] = useState(false);
  /** 授权链接是否刚被复制（按钮回显用） */
  const [urlCopied, setUrlCopied] = useState(false);

  /** 轮询定时器：对话框关闭或授权完成时必须清掉，否则会一直打接口 */
  const pollTimer = useRef<number | null>(null);

  // onClose 每次渲染都是新函数，成功后定时关闭要用 ref 取最新值，
  // 避免把 onClose 放进依赖导致定时器被反复重置
  const onCloseRef = useRef(onClose);
  useEffect(() => {
    onCloseRef.current = onClose;
  }, [onClose]);

  /** 登录/授权成功（phase=done）后自动收起：留 1.2s 让用户看到成功提示再关闭 */
  useEffect(() => {
    if (phase !== 'done') return;
    const timer = window.setTimeout(() => onCloseRef.current(), 1200);
    return () => window.clearTimeout(timer);
  }, [phase]);

  const stopPolling = useCallback(() => {
    if (pollTimer.current !== null) {
      window.clearInterval(pollTimer.current);
      pollTimer.current = null;
    }
  }, []);

  /** 拉取所选区域支持的登录方式 */
  const loadProviders = useCallback(async (targetRegion: 'global' | 'cn') => {
    try {
      const response = await fetch(`${CINDY_BASE}/api/login/providers?region=${targetRegion}`);
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const data = (await response.json()) as ProvidersResponse;
      setProviders(data);
      // 默认选中 Google。注意不能取 social[0] —— 服务端返回的是 ["apple","google"]，
      // 取第一个会默认落到 Apple 上。
      const social = data.social ?? [];
      setProvider(social.includes('google') ? 'google' : (social[0] ?? ''));
      if (!data.social || data.social.length === 0) {
        // 优先用服务端给出的口径说明（例如 CN 区域是「86 手机号 + 企业 SSO」），
        // 比前端自己写一句通用文案准确
        setMessage(
          data.desktopAuthorizationHint ??
            '该区域未开放社交登录，请改用「本机导入」页签。',
        );
      } else {
        setMessage('');
      }
    } catch (error) {
      setProviders(null);
      setMessage(`无法读取登录方式：${error instanceof Error ? error.message : String(error)}`);
    }
  }, []);

  useEffect(() => {
    if (!open) {
      stopPolling();
      setPhase('idle');
      setMessage('');
      setAuthorizeUrl('');
      setUrlCopied(false);
      // 关弹窗时顺手结束授权窗口状态，避免残留窗口被误认为仍在登录
      void closeOAuthWindow().catch(() => {});
      return;
    }
    void loadProviders(region);
  }, [open, region, loadProviders, stopPolling]);

  useEffect(() => stopPolling, [stopPolling]);

  /**
   * 用可信授权窗口打开授权页。
   *
   * 失败不中止流程：授权在服务端是会话级的，用户把链接复制到浏览器里完成也一样能被
   * 轮询取到，所以这里只改提示文案，不动 phase（保持 waiting，轮询照跑）。
   */
  const openAuthWindow = useCallback(async (url: string) => {
    if (!url) return;
    try {
      await openOAuthWindow(url);
      setMessage('已打开可信授权窗口，请在窗口中完成授权…');
    } catch (error) {
      const detail = error instanceof Error ? error.message : String(error);
      setMessage(`打开可信授权窗口失败：${detail}。可复制下方链接到浏览器打开。`);
    }
  }, []);

  /** 复制授权链接（授权窗口打不开时的退路） */
  const copyAuthorizeUrl = useCallback(async () => {
    try {
      await navigator.clipboard.writeText(authorizeUrl);
      setUrlCopied(true);
      window.setTimeout(() => setUrlCopied(false), 1200);
    } catch {
      setMessage('复制失败，请手动选中下方链接复制。');
    }
  }, [authorizeUrl]);

  /** 发起授权：sidecar 只返回地址（openBrowser=false），授权页由本组件打开 */
  const startAuthorization = useCallback(async () => {
    stopPolling();
    setPhase('waiting');
    setMessage('正在发起授权…');
    try {
      const isSso = provider === 'sso';
      const response = await fetch(`${CINDY_BASE}/api/login/oauth/start`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          kind: isSso ? 'sso' : 'social',
          // SSO 走组织标识，社交登录走 provider 名
          provider: isSso ? ssoOrg.trim() : provider,
          region,
          // 不要让 sidecar 拉起系统浏览器：它复用的登录态会导致再次授权还是上一个账号
          openBrowser: false,
        }),
      });
      const data = (await response.json()) as { sessionId?: string; authorizeUrl?: string; error?: { message?: string } };
      if (!response.ok || !data.sessionId) {
        setPhase('error');
        setMessage(data.error?.message ?? `发起授权失败（HTTP ${response.status}）`);
        return;
      }
      setAuthorizeUrl(data.authorizeUrl ?? '');

      // 用可信授权窗口打开（而不是 sidecar 自行拉起浏览器）
      await openAuthWindow(data.authorizeUrl ?? '');      // 完成授权需要时间，2 秒一轮；终态由 sidecar 判定
      pollTimer.current = window.setInterval(async () => {
        try {
          const pollResponse = await fetch(`${CINDY_BASE}/api/login/oauth/poll`, {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ sessionId: data.sessionId }),
          });
          const pollData = (await pollResponse.json()) as {
            status?: string;
            label?: string;
            endpoint?: string;
            /** true = 取回的是上一次未取走的授权结果 */
            reusedPreviousAuthorization?: boolean;
            error?: { message?: string };
          };
          if (pollResponse.status === 410) {
            stopPolling();
            setPhase('error');
            setMessage(pollData.error?.message ?? '授权已过期，请重新发起。');
            return;
          }
          if (!pollResponse.ok) {
            stopPolling();
            setPhase('error');
            setMessage(pollData.error?.message ?? `授权失败（HTTP ${pollResponse.status}）`);
            return;
          }
          if (pollData.status === 'ok') {
            stopPolling();
            setPhase('done');
            // 极短时间内返回结果 = 取回的是上次浏览器里已完成、当时没被取走的授权码。
            // 如实说明，否则用户会以为"没进授权页就成功了"。
            setMessage(
              pollData.reusedPreviousAuthorization
                ? `已取回上一次未完成的授权结果，账号已添加：${pollData.label ?? ''}`
                : `授权成功，已添加账号：${pollData.label ?? ''}`,
            );
            onAdded();
          }
        } catch (error) {
          stopPolling();
          setPhase('error');
          setMessage(`轮询失败：${error instanceof Error ? error.message : String(error)}`);
        }
      }, 2000);
    } catch (error) {
      setPhase('error');
      setMessage(`发起授权失败：${error instanceof Error ? error.message : String(error)}`);
    }
  }, [provider, ssoOrg, region, onAdded, stopPolling, openAuthWindow]);

  /** 本机导入：让 sidecar 重新扫描本机 Cindy 登录态 */
  const importLocal = useCallback(async () => {
    setImporting(true);
    setMessage('');
    try {
      const response = await fetch(`${CINDY_BASE}/api/refresh`, { method: 'POST' });
      const data = (await response.json()) as { total?: number; added?: number };
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      setMessage(
        data.added && data.added > 0
          ? `已从本机 Cindy 登录态导入 ${data.added} 个新账号，共 ${data.total} 个。`
          : `本机登录态已是最新，共 ${data.total} 个账号。`,
      );
      onAdded();
    } catch (error) {
      setMessage(`导入失败：${error instanceof Error ? error.message : String(error)}`);
    } finally {
      setImporting(false);
    }
  }, [onAdded]);

  if (!open) return null;

  const socialList = providers?.social ?? [];

  return (
    <div
      style={{
        position: 'fixed',
        inset: 0,
        background: 'rgba(0,0,0,0.55)',
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        zIndex: 1000,
      }}
      onClick={() => {
        stopPolling();
        onClose();
      }}
    >
      <div
        style={{
          width: 620,
          maxWidth: '92vw',
          background: 'var(--surface-secondary)',
          border: '1px solid var(--border-subtle)',
          borderRadius: 16,
          padding: '24px 26px',
          color: 'var(--text-primary)',
        }}
        onClick={(event) => event.stopPropagation()}
      >
        <header style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 18 }}>
          <h3 style={{ margin: 0, fontSize: 18 }}>添加 Cindy 账号</h3>
          <button
            onClick={() => {
              stopPolling();
              onClose();
            }}
            style={{ background: 'transparent', border: 0, color: 'var(--text-secondary)', cursor: 'pointer' }}
          >
            <X size={18} />
          </button>
        </header>

        {/* 页签 */}
        <div style={{ display: 'flex', gap: 8, marginBottom: 18 }}>
          {(
            [
              ['phone', '国内登录', <Smartphone size={15} key="p" />],
              ['global', '国际登录', <Globe size={15} key="g" />],
              ['local', '本机导入', <MonitorSmartphone size={15} key="m" />],
            ] as const
          ).map(([key, label, icon]) => (
            <button
              key={key}
              onClick={() => setTab(key)}
              style={{
                flex: 1,
                display: 'flex',
                alignItems: 'center',
                justifyContent: 'center',
                gap: 6,
                padding: '10px 12px',
                borderRadius: 10,
                cursor: 'pointer',
                border: '1px solid ' + (tab === key ? 'var(--primary, #2f6df6)' : 'var(--border-subtle)'),
                background: tab === key ? 'var(--primary, #2f6df6)' : 'transparent',
                color: tab === key ? '#fff' : 'var(--text-secondary)',
              }}
            >
              {icon}
              {label}
            </button>
          ))}
        </div>

        {tab === 'phone' ? (
          <CindyPhoneLoginForm
            region="cn"
            onSuccess={onAdded}
            onMessage={(text, tone) => {
              setMessage(text);
              setPhase(tone === 'error' ? 'error' : tone === 'done' ? 'done' : 'idle');
            }}
          />
        ) : tab === 'global' ? (
          <>
            {/* 国际登录的两种方式并列切换：邮箱验证码 / 第三方授权 */}
            <div style={{ display: 'flex', gap: 8, marginBottom: 14 }}>
              {(
                [
                  ['email', '邮箱登录'],
                  ['oauth', 'OAuth 授权'],
                ] as const
              ).map(([key, label]) => {
                const active = globalMode === key;
                return (
                  <button
                    key={key}
                    onClick={() => {
                      setGlobalMode(key);
                      setMessage('');
                      setPhase('idle');
                    }}
                    style={{
                      flex: 1,
                      padding: '9px 12px',
                      borderRadius: 9,
                      cursor: 'pointer',
                      border:
                        '1px solid ' + (active ? 'var(--primary, #2f6df6)' : 'var(--border-subtle)'),
                      background: active ? 'var(--surface-tertiary)' : 'transparent',
                      color: active ? 'var(--text-primary)' : 'var(--text-secondary)',
                      fontWeight: active ? 600 : 400,
                      fontSize: 13,
                    }}
                  >
                    {label}
                  </button>
                );
              })}
            </div>

            {globalMode === 'email' ? (
              <CindyEmailLoginForm
                region={region}
                onSuccess={onAdded}
                onMessage={(text, tone) => {
                  setMessage(text);
                  setPhase(tone === 'error' ? 'error' : tone === 'done' ? 'done' : 'idle');
                }}
              />
            ) : (
              <>
            <p style={{ color: 'var(--text-secondary)', fontSize: 13, marginTop: 0 }}>
              使用国际版账号授权登录（Apple / Google）。点击下方按钮会在可信授权窗口中
              打开 Cindy 授权页（复用本机可信登录态），
              完成授权后本窗口自动更新。
            </p>

            {/* 登录方式全部平铺成可点选块，一眼看完、单击切换 */}
            <div style={{ display: 'flex', gap: 8, marginBottom: 14 }}>
              {authOptions(socialList).map((option) => {
                const active = provider === option.key;
                return (
                  <button
                    key={option.key}
                    onClick={() => setProvider(option.key)}
                    style={{
                      flex: 1,
                      padding: '10px 12px',
                      borderRadius: 10,
                      cursor: 'pointer',
                      border: '1px solid ' + (active ? 'var(--primary, #2f6df6)' : 'var(--border-subtle)'),
                      background: active ? 'var(--primary, #2f6df6)' : 'transparent',
                      color: active ? '#fff' : 'var(--text-secondary)',
                      fontWeight: active ? 600 : 400,
                    }}
                  >
                    {option.label}
                  </button>
                );
              })}
            </div>

            {provider === 'sso' && (
              <input
                value={ssoOrg}
                onChange={(event) => setSsoOrg(event.target.value)}
                placeholder="企业 SSO 组织标识（如 acme）"
                style={{
                  width: '100%',
                  padding: '11px 12px',
                  borderRadius: 9,
                  background: 'var(--surface-tertiary)',
                  border: '1px solid var(--border-subtle)',
                  color: 'var(--text-primary)',
                  marginBottom: 12,
                }}
              />
            )}

            {authorizeUrl && (
                  <div
                    style={{
                      fontSize: 12,
                      color: 'var(--text-secondary)',
                      wordBreak: 'break-all',
                      marginBottom: 12,
                      fontFamily: 'var(--font-mono, monospace)',
                    }}
                  >
                    {authorizeUrl}
                  </div>
                )}

                <button
                  onClick={() => void startAuthorization()}
                  disabled={!provider || (provider === 'sso' && !ssoOrg.trim()) || phase === 'waiting'}
                  style={{
                    width: '100%',
                    padding: '12px',
                    borderRadius: 10,
                    border: 0,
                    cursor:
                      provider && !(provider === 'sso' && !ssoOrg.trim()) && phase !== 'waiting'
                        ? 'pointer'
                        : 'not-allowed',
                    background: provider ? 'var(--primary, #2f6df6)' : 'var(--surface-tertiary)',
                    color: provider ? '#fff' : 'var(--text-secondary)',
                    display: 'flex',
                    alignItems: 'center',
                    justifyContent: 'center',
                    gap: 8,
                    fontWeight: 600,
                  }}
                >
                  {phase === 'waiting' ? <Loader size={16} className="spin" /> : <ExternalLink size={16} />}
                  {phase === 'waiting' ? '等待授权完成…' : '打开可信授权窗口'}
                </button>

                {/* 等待期间窗口可能被用户关掉，给一个重开入口；再给一条复制链接的退路 */}
                {phase === 'waiting' && authorizeUrl && (
                  <div style={{ display: 'flex', gap: 8, marginTop: 8 }}>
                    <button
                      onClick={() => void openAuthWindow(authorizeUrl)}
                      style={{
                        flex: 1,
                        padding: '9px 10px',
                        borderRadius: 9,
                        border: '1px solid var(--border-subtle)',
                        background: 'transparent',
                        color: 'var(--text-secondary)',
                        cursor: 'pointer',
                        display: 'flex',
                        alignItems: 'center',
                        justifyContent: 'center',
                        gap: 6,
                        fontSize: 12,
                      }}
                    >
                      <ExternalLink size={14} />
                      重新打开授权窗口
                    </button>
                    <button
                      onClick={() => void copyAuthorizeUrl()}
                      style={{
                        flex: 1,
                        padding: '9px 10px',
                        borderRadius: 9,
                        border: '1px solid var(--border-subtle)',
                        background: 'transparent',
                        color: 'var(--text-secondary)',
                        cursor: 'pointer',
                        display: 'flex',
                        alignItems: 'center',
                        justifyContent: 'center',
                        gap: 6,
                        fontSize: 12,
                      }}
                    >
                      {urlCopied ? <Check size={14} /> : <Copy size={14} />}
                      {urlCopied ? '已复制' : '复制授权链接'}
                    </button>
                  </div>
                )}
              </>
            )}
          </>
        ) : (
          <>
            <p style={{ color: 'var(--text-secondary)', fontSize: 13, marginTop: 0 }}>
              直接读取本机 Cindy 桌面端已登录的账号（含 dev / 隔离 profile），
              <strong>不需要授权、不需要 Cindy 保持运行</strong>。
            </p>
            <button
              onClick={() => void importLocal()}
              disabled={importing}
              style={{
                width: '100%',
                padding: '12px',
                borderRadius: 10,
                border: 0,
                cursor: importing ? 'not-allowed' : 'pointer',
                background: 'var(--primary, #2f6df6)',
                color: '#fff',
                display: 'flex',
                alignItems: 'center',
                justifyContent: 'center',
                gap: 8,
                fontWeight: 600,
              }}
            >
              {importing ? <Loader size={16} className="spin" /> : <MonitorSmartphone size={16} />}
              {importing ? '扫描中…' : '扫描本机登录态'}
            </button>
          </>
        )}

        {message && (
          <div
            style={{
              marginTop: 14,
              padding: '10px 12px',
              borderRadius: 9,
              fontSize: 13,
              background: 'var(--surface-tertiary)',
              color: phase === 'done' ? '#56e0b0' : 'var(--text-secondary)',
              display: 'flex',
              alignItems: 'center',
              gap: 8,
            }}
          >
            {phase === 'done' && <Check size={15} />}
            {message}
          </div>
        )}
      </div>
    </div>
  );
}
