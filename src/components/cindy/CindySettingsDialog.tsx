/**
 * CindySettingsDialog —— Cindy 自己的设置弹窗（复刻 ZCode 设置的形态）。
 *
 * 刻意独立实现、不扩共享的 `QuickSettingsPopover`：
 * 那个组件的 `type` 是写死的联合类型（codebuddy_cn / workbuddy / zcode / grok），
 * 为 Cindy 加成员就得改共享文件，会给上游合并埋冲突。
 *
 * 原则：**这里每一个开关都真实生效**，不做摆设。
 *   - 自动刷新间隔 → 账号列表按该间隔重新拉取（CindyAccountsView 里实现）
 *   - 每页条数     → 卡片分页大小
 *   - 视图模式     → 网格 / 列表
 * 保存后广播 `cindy-settings-changed`，视图侧监听并即时生效。
 */
import { useCallback, useEffect, useState } from 'react';
import { FolderOpen, RefreshCw, Settings, X } from 'lucide-react';

import { CINDY_GATEWAY_BASE } from '../../services/cindyService';

export const CINDY_SETTINGS_KEY = 'agtools.cindy.settings';
export const CINDY_SETTINGS_CHANGED_EVENT = 'cindy-settings-changed';

export interface CindySettings {
  /** 账号列表自动刷新间隔（秒）；0 = 关闭 */
  autoRefreshSeconds: number;
  /** 每页条数 */
  pageSize: number;
  /** 是否记忆视图模式与筛选 */
  rememberFilters: boolean;
}

export const DEFAULT_CINDY_SETTINGS: CindySettings = {
  autoRefreshSeconds: 600,
  pageSize: 20,
  rememberFilters: true,
};

/** 读取设置（任何异常都退化为默认值） */
export function readCindySettings(): CindySettings {
  try {
    const raw = window.localStorage.getItem(CINDY_SETTINGS_KEY);
    if (!raw) return DEFAULT_CINDY_SETTINGS;
    const parsed = JSON.parse(raw) as Partial<CindySettings>;
    return {
      autoRefreshSeconds:
        typeof parsed.autoRefreshSeconds === 'number'
          ? parsed.autoRefreshSeconds
          : DEFAULT_CINDY_SETTINGS.autoRefreshSeconds,
      pageSize:
        typeof parsed.pageSize === 'number' && parsed.pageSize > 0
          ? parsed.pageSize
          : DEFAULT_CINDY_SETTINGS.pageSize,
      rememberFilters:
        typeof parsed.rememberFilters === 'boolean'
          ? parsed.rememberFilters
          : DEFAULT_CINDY_SETTINGS.rememberFilters,
    };
  } catch {
    return DEFAULT_CINDY_SETTINGS;
  }
}

const REFRESH_OPTIONS: Array<{ label: string; value: number }> = [
  { label: '关闭', value: 0 },
  { label: '1 分钟', value: 60 },
  { label: '5 分钟', value: 300 },
  { label: '10 分钟', value: 600 },
  { label: '30 分钟', value: 1800 },
];

const PAGE_SIZE_OPTIONS = [10, 20, 50];

interface Props {
  open: boolean;
  onClose: () => void;
  onSaved?: () => void;
}

export function CindySettingsDialog({ open, onClose, onSaved }: Props) {
  const [draft, setDraft] = useState<CindySettings>(DEFAULT_CINDY_SETTINGS);

  // 每次打开都从磁盘读一遍，避免显示过期值
  useEffect(() => {
    if (open) setDraft(readCindySettings());
  }, [open]);

  const save = useCallback(() => {
    try {
      window.localStorage.setItem(CINDY_SETTINGS_KEY, JSON.stringify(draft));
      window.dispatchEvent(new Event(CINDY_SETTINGS_CHANGED_EVENT));
    } catch {
      /* localStorage 不可用时忽略 */
    }
    onSaved?.();
    onClose();
  }, [draft, onSaved, onClose]);

  if (!open) return null;

  const selectStyle = {
    width: '100%',
    marginTop: 8,
    padding: '10px 12px',
    borderRadius: 9,
    background: 'var(--surface-tertiary)',
    border: '1px solid var(--border-subtle)',
    color: 'var(--text-primary)',
  } as const;

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
      onClick={onClose}
    >
      <div
        style={{
          width: 560,
          maxWidth: '92vw',
          maxHeight: '86vh',
          overflow: 'auto',
          background: 'var(--surface-secondary)',
          border: '1px solid var(--border-subtle)',
          borderRadius: 16,
          padding: '24px 26px',
          color: 'var(--text-primary)',
        }}
        onClick={(event) => event.stopPropagation()}
      >
        <header style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center', marginBottom: 20 }}>
          <h3 style={{ margin: 0, fontSize: 18 }}>Cindy 设置</h3>
          <button onClick={onClose} style={{ background: 'transparent', border: 0, color: 'var(--text-secondary)', cursor: 'pointer' }}>
            <X size={18} />
          </button>
        </header>

        {/* 账号自动刷新 */}
        <section style={{ marginBottom: 22 }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 8, color: 'var(--text-secondary)', fontSize: 13 }}>
            <RefreshCw size={15} /> 账号自动刷新
          </div>
          <select
            value={draft.autoRefreshSeconds}
            onChange={(event) => setDraft({ ...draft, autoRefreshSeconds: Number(event.target.value) })}
            style={selectStyle}
          >
            {REFRESH_OPTIONS.map((option) => (
              <option key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </select>
          <p style={{ fontSize: 12, color: 'var(--text-secondary)', margin: '8px 0 0' }}>
            按该间隔重新读取账号状态；默认 10 分钟。探测会实际请求上游，间隔过短没有意义。
          </p>
        </section>

        {/* 每页条数 */}
        <section style={{ marginBottom: 22 }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 8, color: 'var(--text-secondary)', fontSize: 13 }}>
            <Settings size={15} /> 每页显示
          </div>
          <select
            value={draft.pageSize}
            onChange={(event) => setDraft({ ...draft, pageSize: Number(event.target.value) })}
            style={selectStyle}
          >
            {PAGE_SIZE_OPTIONS.map((size) => (
              <option key={size} value={size}>
                {size} 条 / 页
              </option>
            ))}
          </select>
        </section>

        {/* 筛选记忆 */}
        <section style={{ marginBottom: 22 }}>
          <div style={{ display: 'flex', justifyContent: 'space-between', alignItems: 'center' }}>
            <span style={{ fontSize: 14 }}>记住筛选与视图模式</span>
            <input
              type="checkbox"
              checked={draft.rememberFilters}
              onChange={(event) => setDraft({ ...draft, rememberFilters: event.target.checked })}
              style={{ width: 18, height: 18, cursor: 'pointer' }}
            />
          </div>
          <p style={{ fontSize: 12, color: 'var(--text-secondary)', margin: '8px 0 0' }}>
            开启后会按平台记住搜索词、视图模式与排序。
          </p>
        </section>

        {/* 网关服务（只读） */}
        <section style={{ marginBottom: 24 }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 8, color: 'var(--text-secondary)', fontSize: 13 }}>
            <FolderOpen size={15} /> 反代网关服务
          </div>
          <div
            style={{
              ...selectStyle,
              fontFamily: 'var(--font-mono, monospace)',
              fontSize: 12,
              wordBreak: 'break-all',
            }}
          >
            {CINDY_GATEWAY_BASE}
          </div>
          <p style={{ fontSize: 12, color: 'var(--text-secondary)', margin: '8px 0 0' }}>
            由 CockpitTools 托管启动（sidecars/cindy2api），监听地址可在其
            <code> runtime/config.json </code> 中调整。账号凭据只保存在本机。
          </p>
        </section>

        <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 10 }}>
          <button
            onClick={onClose}
            style={{
              padding: '10px 18px',
              borderRadius: 9,
              border: '1px solid var(--border-subtle)',
              background: 'transparent',
              color: 'var(--text-primary)',
              cursor: 'pointer',
            }}
          >
            取消
          </button>
          <button
            onClick={save}
            style={{
              padding: '10px 18px',
              borderRadius: 9,
              border: 0,
              background: 'var(--primary, #2f6df6)',
              color: '#fff',
              cursor: 'pointer',
              fontWeight: 600,
            }}
          >
            保存
          </button>
        </div>
      </div>
    </div>
  );
}
