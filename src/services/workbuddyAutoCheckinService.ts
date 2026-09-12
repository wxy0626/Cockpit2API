import { invoke } from '@tauri-apps/api/core';

/**
 * WorkBuddy 自动签到服务（前端封装层）。
 * 调度与执行全部在 Rust 后端（workbuddy_auto_checkin.rs），
 * 本文件只负责配置读写、日志读取与手动触发，不再包含任何前端调度逻辑。
 */

export interface WorkbuddyAccountScheduleState {
  scheduledDate: string;        // "YYYY-MM-DD"
  scheduledMinute: number;      // Minutes from midnight (0..1439)
  lastCheckedDate?: string;     // "YYYY-MM-DD" when checked in
}

export interface WorkbuddyAutoCheckinConfig {
  enabled: boolean;
  startTime: string; // HH:mm, e.g. "06:00"
  endTime: string;   // HH:mm, e.g. "12:00"
  lastCheckedDate?: string; // "YYYY-MM-DD"
  accountSchedules?: Record<string, WorkbuddyAccountScheduleState>;
}

export const DEFAULT_WORKBUDDY_AUTO_CHECKIN_CONFIG: WorkbuddyAutoCheckinConfig = {
  enabled: false,
  startTime: '06:00',
  endTime: '12:00',
};

const CONFIG_KEY = 'agtools.workbuddy.auto_checkin_config';
const LEGACY_LOGS_KEY = 'agtools.workbuddy.auto_checkin_logs';
export const WORKBUDDY_AUTO_CHECKIN_CONFIG_CHANGED_EVENT = 'workbuddy-auto-checkin-config-changed';

export type WorkbuddyAutoCheckinCycleResult = 'disabled' | 'waiting' | 'completed' | 'retry';

/** 清理旧版本遗留在 WebView localStorage 中的自动签到日志缓存 */
export function clearLegacyWorkbuddyAutoCheckinLogs(): void {
  if (typeof window === 'undefined') {
    return;
  }

  try {
    if (localStorage.getItem(LEGACY_LOGS_KEY) !== null) {
      localStorage.removeItem(LEGACY_LOGS_KEY);
      console.info('[WorkbuddyAutoCheckin] 已清理废弃的 WebView 自动签到日志缓存');
    }
  } catch (err) {
    console.warn('[WorkbuddyAutoCheckin] 清理废弃的自动签到日志缓存失败:', err);
  }
}

let cachedConfig: WorkbuddyAutoCheckinConfig | null = null;

function isValidTime(time: unknown): time is string {
  return typeof time === 'string' && /^([01]\d|2[0-3]):[0-5]\d$/.test(time);
}

/** 更新内存缓存并写入 localStorage（可选派发配置变更事件） */
function cacheConfigLocally(config: WorkbuddyAutoCheckinConfig, emitChange = false): void {
  cachedConfig = config;
  if (typeof window === 'undefined') {
    return;
  }
  try {
    localStorage.setItem(CONFIG_KEY, JSON.stringify(config));
    if (emitChange) {
      window.dispatchEvent(new Event(WORKBUDDY_AUTO_CHECKIN_CONFIG_CHANGED_EVENT));
    }
  } catch (err) {
    console.warn('[WorkbuddyAutoCheckin] 本地缓存保存失败:', err);
  }
}

/** 从 Rust 后端读取自动签到配置，失败时回退到本地缓存或默认值 */
export async function getWorkbuddyAutoCheckinConfigAsync(): Promise<WorkbuddyAutoCheckinConfig> {
  try {
    const config = await invoke<WorkbuddyAutoCheckinConfig>('get_workbuddy_auto_checkin_config');
    cacheConfigLocally(config);
    return config;
  } catch (err) {
    console.warn('[WorkbuddyAutoCheckin] 从 Rust 端获取配置失败，使用本地缓存或默认值:', err);
    return getWorkbuddyAutoCheckinConfig();
  }
}

/** 同步读取配置：优先内存缓存，其次 localStorage，最后默认值 */
export function getWorkbuddyAutoCheckinConfig(): WorkbuddyAutoCheckinConfig {
  if (cachedConfig) {
    return cachedConfig;
  }
  if (typeof window === 'undefined') {
    return DEFAULT_WORKBUDDY_AUTO_CHECKIN_CONFIG;
  }
  try {
    const raw = localStorage.getItem(CONFIG_KEY);
    if (!raw) {
      return DEFAULT_WORKBUDDY_AUTO_CHECKIN_CONFIG;
    }
    const parsed = JSON.parse(raw);
    const config: WorkbuddyAutoCheckinConfig = {
      enabled: typeof parsed.enabled === 'boolean' ? parsed.enabled : false,
      startTime: isValidTime(parsed.startTime) ? parsed.startTime : '06:00',
      endTime: isValidTime(parsed.endTime) ? parsed.endTime : '12:00',
      lastCheckedDate: typeof parsed.lastCheckedDate === 'string' ? parsed.lastCheckedDate : undefined,
      accountSchedules: typeof parsed.accountSchedules === 'object' && parsed.accountSchedules !== null ? parsed.accountSchedules : undefined,
    };
    cachedConfig = config;
    return config;
  } catch {
    return DEFAULT_WORKBUDDY_AUTO_CHECKIN_CONFIG;
  }
}

/** 一次性迁移：把旧版 localStorage 配置交给 Rust 后端托管 */
export async function migrateWorkbuddyAutoCheckinConfigAsync(
  legacyConfig: WorkbuddyAutoCheckinConfig,
): Promise<WorkbuddyAutoCheckinConfig> {
  const config = await invoke<WorkbuddyAutoCheckinConfig>(
    'migrate_workbuddy_auto_checkin_config',
    { legacyConfig },
  );
  cacheConfigLocally(config, true);
  return config;
}

/** 保存配置到 Rust 后端（并同步本地缓存与变更事件） */
export async function saveWorkbuddyAutoCheckinConfigAsync(config: WorkbuddyAutoCheckinConfig): Promise<void> {
  if (typeof window === 'undefined') {
    cacheConfigLocally(config);
    return;
  }
  await invoke('save_workbuddy_auto_checkin_config', { config });
  cacheConfigLocally(config, true);
}

/** 时间字符串转分钟数（"06:30" → 390），供设置弹窗校验使用 */
export function parseTimeToMinutes(timeStr: string): number {
  const parts = timeStr.split(':').map(Number);
  const h = parts[0] ?? 0;
  const m = parts[1] ?? 0;
  return h * 60 + m;
}

/** 本地日期字符串（YYYY-MM-DD），用于判断排期/签到时间是否属于今天 */
export function getLocalTodayStr(): string {
  const now = new Date();
  const month = String(now.getMonth() + 1).padStart(2, '0');
  const day = String(now.getDate()).padStart(2, '0');
  return `${now.getFullYear()}-${month}-${day}`;
}

/** 分钟数（0..1439）转 "HH:mm"（如 551 → "09:11"） */
export function formatMinuteOfDay(minute: number): string {
  const h = Math.floor(minute / 60) % 24;
  const m = minute % 60;
  return `${String(h).padStart(2, '0')}:${String(m).padStart(2, '0')}`;
}

/** Unix 秒时间戳转 "HH:mm:ss"，仅当属于今天时返回（旧时间不展示） */
export function formatTodayTimestamp(ts?: number | null): string | undefined {
  if (!ts || ts <= 0) {
    return undefined;
  }
  const date = new Date(ts * 1000);
  if (date.toDateString() !== new Date().toDateString()) {
    return undefined;
  }
  const h = String(date.getHours()).padStart(2, '0');
  const m = String(date.getMinutes()).padStart(2, '0');
  const s = String(date.getSeconds()).padStart(2, '0');
  return `${h}:${m}:${s}`;
}

export interface WorkbuddyAutoCheckinAccountDetail {
  accountId: string;
  email: string;
  status: 'success' | 'already_checked' | 'failed' | 'inactive';
  time?: string;
  message?: string;
  credit?: number;
}

export interface WorkbuddyAutoCheckinLogRecord {
  id: string;
  timestamp: string;
  date: string;
  durationMs: number;
  totalAccounts: number;
  successCount: number;
  alreadyCheckedCount: number;
  failedCount: number;
  status: 'success' | 'partial' | 'failed' | 'no_accounts';
  details: WorkbuddyAutoCheckinAccountDetail[];
}

export const WORKBUDDY_AUTO_CHECKIN_LOGS_CHANGED_EVENT = 'workbuddy-auto-checkin-logs-changed';

/** 读取近 30 天自动签到记录（数据源在 Rust 后端） */
export async function getWorkbuddyAutoCheckinLogsAsync(): Promise<WorkbuddyAutoCheckinLogRecord[]> {
  return await invoke<WorkbuddyAutoCheckinLogRecord[]>('get_workbuddy_auto_checkin_logs');
}

/** 清空自动签到记录 */
export async function clearWorkbuddyAutoCheckinLogs(): Promise<void> {
  await invoke('clear_workbuddy_auto_checkin_logs');
}

/** 手动触发一次自动签到流程（force=true 时忽略当日已签限制） */
export async function runWorkbuddyAutoCheckinCycleIfNeeded(
  force = false,
): Promise<WorkbuddyAutoCheckinCycleResult> {
  const res = await invoke<string>('run_workbuddy_auto_checkin_now', { force });
  if (res === 'already_running') {
    return 'waiting';
  }
  if (res === 'disabled' || res === 'waiting' || res === 'completed' || res === 'retry') {
    return res as WorkbuddyAutoCheckinCycleResult;
  }
  return 'completed';
}
