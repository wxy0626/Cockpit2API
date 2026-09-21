/**
 * CindyEmailLoginForm —— 邮箱 + 邮箱验证码登录（国际版）。
 *
 * 走的是 Cindy 官方登录接口（与手机号流同构）：
 *   POST {authBase}/api/auth/email/request-code  {email, captchaToken?, locale}
 *   POST {authBase}/api/auth/email/verify-code   {email, code, deviceId, ...}
 *
 * 人机验证为什么必须借官方验证页（而不是本页面内嵌组件）：
 * 上游 siteverify 会校验 token 的签发环境。我们自渲染（缺 action/cData，
 * 且页面主机名不是官方域名）拿到的 token 一律被判 CAPTCHA_INVALID（实测）。
 * 官方客户端也是开一个独立窗口加载 `https://auth.cindy.app/captcha/turnstile`，
 * 再通过桥/hash 把 token 取回 —— 见 src-tauri/src/modules/cindy_captcha_window.rs。
 *
 * 前端只负责：登记会话 → 唤起那个窗口 → 轮询结果。
 */
import { useCallback, useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { Loader, Mail } from 'lucide-react';

/** sidecar 默认监听地址 */
const CINDY_BASE = 'http://127.0.0.1:7865';

/** 验证码重发冷却（秒）——与手机号表单一致，避免用户连点 */
const RESEND_COOLDOWN = 60;

/** 验证窗口流程整体超时（毫秒）：含 Cloudflare 可能弹出的手动挑战时间 */
const CAPTCHA_FLOW_TIMEOUT = 150_000;

/** 生成一次验证流程的会话 id */
function newSessionId(): string {
  if (typeof crypto !== 'undefined' && crypto.randomUUID) return crypto.randomUUID();
  return `s-${Date.now()}-${Math.random().toString(36).slice(2)}`;
}

interface Props {
  region: 'global' | 'cn';
  /** 登录成功（账号已由 sidecar 落库） */
  onSuccess: () => void;
  /** 把提示消息交给父组件统一展示 */
  onMessage: (message: string, tone: 'info' | 'error' | 'done') => void;
}

export function CindyEmailLoginForm({ region, onSuccess, onMessage }: Props) {
  const [email, setEmail] = useState('');
  const [code, setCode] = useState('');
  const [countdown, setCountdown] = useState(0);
  const [sending, setSending] = useState(false);
  const [verifying, setVerifying] = useState(false);

  /** 重发倒计时 */
  useEffect(() => {
    if (countdown <= 0) return;
    const timer = window.setTimeout(() => setCountdown((value) => value - 1), 1000);
    return () => window.clearTimeout(timer);
  }, [countdown]);

  /** 不带 token 直接发；返回 needCaptcha 表示上游要求人机验证 */
  const postRequestCode = useCallback(
    async (captchaToken?: string, session?: string) => {
      const response = await fetch(`${CINDY_BASE}/api/login/email/request-code`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ email: email.trim(), captchaToken, region, session }),
      });
      const data = (await response.json()) as {
        status?: string;
        error?: { code?: string; message?: string };
      };
      if (response.status === 409) {
        return { needCaptcha: true as const, message: data.error?.message ?? '需要完成人机验证' };
      }
      if (!response.ok) {
        throw new Error(data.error?.message ?? `发送失败（HTTP ${response.status}）`);
      }
      return { needCaptcha: false as const, message: '' };
    },
    [email, region],
  );

  /** 轮询辅助页流程结果，直到 sent / error / 超时 */
  const pollCaptchaSession = useCallback(
    (sessionId: string) =>
      new Promise<{ status: 'sent' | 'error'; message: string }>((resolve, reject) => {
        const startedAt = Date.now();
        const timer = window.setInterval(async () => {
          try {
            const response = await fetch(
              `${CINDY_BASE}/api/login/email/captcha/status?session=${encodeURIComponent(sessionId)}`,
            );
            const data = (await response.json()) as { status?: string; message?: string };
            if (data.status === 'sent') {
              window.clearInterval(timer);
              resolve({ status: 'sent', message: '' });
            } else if (data.status === 'error') {
              window.clearInterval(timer);
              resolve({ status: 'error', message: data.message ?? '安全校验未通过' });
            }
          } catch {
            /* 网络抖动：继续轮询，直到总超时 */
          }
          if (Date.now() - startedAt > CAPTCHA_FLOW_TIMEOUT) {
            window.clearInterval(timer);
            reject(new Error('安全校验超时，请重试。'));
          }
        }, 1000);
      }),
    [],
  );

  const sendCode = useCallback(async () => {
    const trimmed = email.trim();
    if (!trimmed) {
      onMessage('请先填写邮箱地址。', 'error');
      return;
    }
    setSending(true);
    try {
      // 1) 先不带人机验证发一次（若上游某天不再要求，用户完全无感）
      const first = await postRequestCode();
      if (first.needCaptcha) {
        // 2) 走官方验证页窗口（token 只有官方页签发的才被上游接受）
        const sessionId = newSessionId();
        const prepare = await fetch(`${CINDY_BASE}/api/login/email/captcha/prepare`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ session: sessionId, email: trimmed, region }),
        });
        if (!prepare.ok) {
          throw new Error(`准备验证会话失败（HTTP ${prepare.status}）`);
        }
        await invoke('cindy_captcha_window_open', { session: sessionId });
        onMessage('已在弹出的窗口中进行安全校验，完成后自动返回…', 'info');
        const outcome = await pollCaptchaSession(sessionId);
        if (outcome.status === 'error') {
          onMessage(outcome.message || '安全校验未通过，请重试。', 'error');
          return;
        }
      }
      setCountdown(RESEND_COOLDOWN);
      onMessage(`验证码已发送至 ${trimmed}，请查收邮件。`, 'info');
    } catch (error) {
      onMessage(error instanceof Error ? error.message : String(error), 'error');
    } finally {
      setSending(false);
    }
  }, [email, postRequestCode, pollCaptchaSession, region, onMessage]);

  const verify = useCallback(async () => {
    const trimmedEmail = email.trim();
    const trimmedCode = code.trim();
    if (!trimmedEmail || !trimmedCode) {
      onMessage('请填写邮箱与验证码。', 'error');
      return;
    }
    setVerifying(true);
    try {
      const response = await fetch(`${CINDY_BASE}/api/login/email/verify-code`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ email: trimmedEmail, code: trimmedCode, region }),
      });
      const data = (await response.json()) as {
        status?: string;
        label?: string;
        endpoint?: string;
        error?: { message?: string };
      };
      if (!response.ok || data.status !== 'ok') {
        onMessage(data.error?.message ?? `登录失败（HTTP ${response.status}）`, 'error');
        return;
      }
      onMessage(`登录成功，已添加账号：${data.label ?? ''}`, 'done');
      setCode('');
      onSuccess();
    } catch (error) {
      onMessage(`登录失败：${error instanceof Error ? error.message : String(error)}`, 'error');
    } finally {
      setVerifying(false);
    }
  }, [email, code, region, onMessage, onSuccess]);

  const inputStyle = {
    flex: 1,
    padding: '11px 12px',
    borderRadius: 9,
    background: 'var(--surface-tertiary)',
    border: '1px solid var(--border-subtle)',
    color: 'var(--text-primary)',
    fontSize: 14,
  } as const;

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 12 }}>
      {/* 邮箱 */}
      <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
        <Mail size={16} style={{ color: 'var(--text-secondary)', flexShrink: 0 }} />
        <input
          value={email}
          onChange={(event) => setEmail(event.target.value)}
          placeholder="国际版邮箱地址（如 you@example.com）"
          inputMode="email"
          autoComplete="email"
          style={inputStyle}
        />
        <button
          onClick={() => void sendCode()}
          disabled={sending || countdown > 0}
          style={{
            padding: '11px 14px',
            borderRadius: 9,
            border: '1px solid var(--border-subtle)',
            background: 'var(--surface-tertiary)',
            color: countdown > 0 ? 'var(--text-secondary)' : 'var(--text-primary)',
            cursor: sending || countdown > 0 ? 'not-allowed' : 'pointer',
            whiteSpace: 'nowrap',
            display: 'flex',
            alignItems: 'center',
            gap: 6,
          }}
        >
          {sending ? <Loader size={14} className="spin" /> : <Mail size={14} />}
          {countdown > 0 ? `${countdown}s` : '获取验证码'}
        </button>
      </div>

      {/* 验证码 */}
      <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
        <Mail size={16} style={{ opacity: 0, flexShrink: 0 }} />
        <input
          value={code}
          onChange={(event) => setCode(event.target.value)}
          placeholder="邮箱验证码"
          inputMode="numeric"
          style={inputStyle}
          onKeyDown={(event) => {
            if (event.key === 'Enter') void verify();
          }}
        />
      </div>

      {/* 人机验证在独立窗口中完成（官方验证页），此处不再内嵌组件 */}
      <button
        onClick={() => void verify()}
        disabled={verifying}
        style={{
          padding: '12px',
          borderRadius: 10,
          border: 0,
          cursor: verifying ? 'not-allowed' : 'pointer',
          background: 'var(--primary, #2f6df6)',
          color: '#fff',
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          gap: 8,
          fontWeight: 600,
        }}
      >
        {verifying ? <Loader size={16} className="spin" /> : <Mail size={16} />}
        {verifying ? '登录中…' : '登录并添加账号'}
      </button>

      <p style={{ fontSize: 12, color: 'var(--text-secondary)', margin: 0, lineHeight: 1.7 }}>
        走的是 Cindy 官方登录接口，验证码由 Cindy 下发到你的邮箱，本工具不接触邮件内容。
        安全校验由 Cloudflare 提供，需要时会弹出一个独立的小窗口（与官方客户端同一套流程），
        完成后自动关闭。登录成功后凭据只保存在本机。
      </p>
    </div>
  );
}
