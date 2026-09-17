/**
 * CindyPhoneLoginForm —— 手机号 + 短信验证码登录（中国大陆版）。
 *
 * 走的是 Cindy 桌面端自己的登录接口，只是把入口搬到了本工具里：
 *   POST {authBase}/api/auth/phone/request-code  {phone, locale}  → 发短信
 *   POST {authBase}/api/auth/phone/verify-code   {phone, code}    → 返回令牌
 *
 * 中国大陆版的 providers 响应里没有 captcha 字段 —— 手机号流不需要人机验证，
 * 所以这里不涉及 Turnstile。
 */
import { useCallback, useEffect, useState } from 'react';
import { Loader, MessageSquare, Smartphone } from 'lucide-react';

/** sidecar 默认监听地址 */
const CINDY_BASE = 'http://127.0.0.1:7865';

/** 验证码重发冷却（秒）——与服务端限制保持一致，避免用户连点 */
const RESEND_COOLDOWN = 60;

interface Props {
  region: 'global' | 'cn';
  /** 登录成功（账号已由 sidecar 落库） */
  onSuccess: () => void;
  /** 把提示消息交给父组件统一展示 */
  onMessage: (message: string, tone: 'info' | 'error' | 'done') => void;
}

export function CindyPhoneLoginForm({ region, onSuccess, onMessage }: Props) {
  const [phone, setPhone] = useState('');
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

  const sendCode = useCallback(async () => {
    const trimmed = phone.trim();
    if (!trimmed) {
      onMessage('请先填写手机号。', 'error');
      return;
    }
    setSending(true);
    try {
      const response = await fetch(`${CINDY_BASE}/api/login/phone/request-code`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ phone: trimmed, region }),
      });
      const data = (await response.json()) as { status?: string; error?: { message?: string } };
      if (!response.ok) {
        onMessage(data.error?.message ?? `发送失败（HTTP ${response.status}）`, 'error');
        return;
      }
      setCountdown(RESEND_COOLDOWN);
      onMessage(`验证码已发送至 ${trimmed}，请查看短信。`, 'info');
    } catch (error) {
      onMessage(`发送失败：${error instanceof Error ? error.message : String(error)}`, 'error');
    } finally {
      setSending(false);
    }
  }, [phone, region, onMessage]);

  const verify = useCallback(async () => {
    const trimmedPhone = phone.trim();
    const trimmedCode = code.trim();
    if (!trimmedPhone || !trimmedCode) {
      onMessage('请填写手机号与验证码。', 'error');
      return;
    }
    setVerifying(true);
    try {
      const response = await fetch(`${CINDY_BASE}/api/login/phone/verify-code`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ phone: trimmedPhone, code: trimmedCode, region }),
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
  }, [phone, code, region, onMessage, onSuccess]);

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
      {/* 手机号 */}
      <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
        <Smartphone size={16} style={{ color: 'var(--text-secondary)', flexShrink: 0 }} />
        <input
          value={phone}
          onChange={(event) => setPhone(event.target.value)}
          placeholder="86 手机号（如 13800138000）"
          inputMode="tel"
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
          {sending ? <Loader size={14} className="spin" /> : <MessageSquare size={14} />}
          {countdown > 0 ? `${countdown}s` : '获取验证码'}
        </button>
      </div>

      {/* 验证码 */}
      <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
        <Smartphone size={16} style={{ opacity: 0, flexShrink: 0 }} />
        <input
          value={code}
          onChange={(event) => setCode(event.target.value)}
          placeholder="短信验证码"
          inputMode="numeric"
          style={inputStyle}
          onKeyDown={(event) => {
            if (event.key === 'Enter') void verify();
          }}
        />
      </div>

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
        {verifying ? <Loader size={16} className="spin" /> : <Smartphone size={16} />}
        {verifying ? '登录中…' : '登录并添加账号'}
      </button>

      <p style={{ fontSize: 12, color: 'var(--text-secondary)', margin: 0, lineHeight: 1.7 }}>
        走的是 Cindy 官方登录接口，验证码由 Cindy 下发，本工具不接触你的短信内容；
        登录成功后凭据只保存在本机。
      </p>
    </div>
  );
}
