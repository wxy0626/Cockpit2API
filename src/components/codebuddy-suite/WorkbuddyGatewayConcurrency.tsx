import { useCallback, useEffect, useState } from 'react';
import { Gauge } from 'lucide-react';

const ADMIN_BASE = 'http://127.0.0.1:7864';

export type WorkbuddyGatewayRealm = 'cn' | 'intl';

export interface WbPoolAccount {
  uid?: string;
  nickname?: string;
  realm?: WorkbuddyGatewayRealm;
  in_flight?: number;
  max_in_flight?: number;
  in_flight_full?: boolean;
  cooling?: boolean;
  disabled?: boolean;
}

interface StatusResponse {
  accounts?: unknown[];
}

/** 读取指定区域的网关账号状态，避免国内版和国际版卡片串用同名账号。 */
export function useWbPoolLimits(realm: WorkbuddyGatewayRealm) {
  const [map, setMap] = useState<Record<string, WbPoolAccount>>({});
  const reload = useCallback(async () => {
    try {
      const s = await fetch(`${ADMIN_BASE}/api/status`).then(
        (r) => r.json() as Promise<StatusResponse>,
      );
      const next: Record<string, WbPoolAccount> = {};
      for (const account of (s.accounts ?? []) as WbPoolAccount[]) {
        if (account.realm === realm && account.uid) {
          next[account.uid] = account;
        }
      }
      setMap(next);
    } catch {
      // 网关未运行：映射留空，卡片输入框仍可编辑并显示区域默认值。
    }
  }, [realm]);

  useEffect(() => {
    void reload();
  }, [reload]);

  return { map, reload };
}

/** 卡片内联并发输入：0 按原值保存，表示不限制，不清除覆盖。 */
export function WbCardLimitInput({
  uid,
  value,
  inFlight,
  onSaved,
}: {
  uid: string;
  value: number;
  inFlight: number;
  onSaved: () => void;
}) {
  const [text, setText] = useState(String(value));
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    setText(String(value));
  }, [value]);

  const save = async () => {
    const n = Math.max(0, Math.min(99, Number(text) || 0));
    if (n === value) return;
    setSaving(true);
    try {
      await fetch(`${ADMIN_BASE}/api/account/max-in-flight`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ uid, limit: n }),
      });
      onSaved();
    } catch {
      // 网关未运行时保存失败，保留当前输入，待用户重试。
    } finally {
      setSaving(false);
    }
  };

  return (
    <span
      style={{ display: 'inline-flex', alignItems: 'center', gap: 3, marginLeft: 'auto' }}
      title={`在途 ${inFlight} · 并发上限（0 = 不限制）`}
    >
      <Gauge size={13} style={{ opacity: 0.6 }} />
      <input
        type="number"
        min={0}
        max={99}
        value={text}
        disabled={saving}
        aria-label="并发上限"
        style={{
          width: 48,
          padding: '2px 4px',
          fontSize: 12,
          borderRadius: 6,
          border: '1px solid rgba(128,128,128,0.35)',
        }}
        onChange={(event) => setText(event.target.value)}
        onBlur={() => void save()}
        onKeyDown={(event) => {
          if (event.key === 'Enter') (event.target as HTMLInputElement).blur();
        }}
      />
    </span>
  );
}
