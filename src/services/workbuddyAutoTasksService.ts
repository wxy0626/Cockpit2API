import { invoke } from '@tauri-apps/api/core';

/**
 * WorkBuddy 自动任务服务（前端封装层）。
 *
 * 调度与执行全部在 Rust 后端（`modules/workbuddy_auto_tasks.rs`），
 * 本文件只负责配置读写、日志读取与手动触发，不含任何前端调度逻辑。
 *
 * 与「自动签到」的关键区别：本模块处理的是成长中心任务与互动玩法，
 * 只调用「领取/动作类」接口，不涉及任何行为遥测上报。
 */

/** 自动任务配置（与 Rust 端 WorkbuddyAutoTasksConfig 字段一一对应） */
export interface WorkbuddyAutoTasksConfig {
  /** 总开关，默认关闭 */
  enabled: boolean;
  /** 每日执行窗口开始时间 "HH:mm" */
  startTime: string;
  /** 每日执行窗口结束时间 "HH:mm" */
  endTime: string;
  /** 自动完成任务：接受未接任务 + 复核进度 + 领取奖励 */
  runTasks: boolean;
  /** 自动玩法（免费收益）：抽奖 / 连签兑换 / 补签卡 / 礼包 / 补偿 */
  runPlay: boolean;
  /** 自动开盲盒（消耗能量，故与上项分开） */
  runBlindbox: boolean;
  /** 自动同意 Buddy 领养协议（任务链的前置，默认开启） */
  autoAdoptAgreement: boolean;
  /** 应用「和平精英」主题完成任务 Hp_Appearance（会真的切换客户端主题，默认关闭） */
  runThemeTask: boolean;
  /** 行为上报通道：为报告型任务上报事件使其能完成（默认关闭，服务端无法核实真实性） */
  enableReporting: boolean;
  /** 最近一次成功执行的本地日期 "YYYY-MM-DD" */
  lastRunDate?: string;
}

export const DEFAULT_WORKBUDDY_AUTO_TASKS_CONFIG: WorkbuddyAutoTasksConfig = {
  enabled: false,
  startTime: '07:00',
  endTime: '12:00',
  runTasks: true,
  runPlay: true,
  runBlindbox: false,
  autoAdoptAgreement: true,
  runThemeTask: false,
  enableReporting: false,
};

/** 单账号执行明细 */
export interface WorkbuddyAutoTasksAccountDetail {
  accountId: string;
  email: string;
  /** 本轮新接受的任务数 */
  acceptedCount: number;
  /** 本轮领取奖励的任务数 */
  claimedCount: number;
  /** 本轮执行的玩法次数 */
  playedCount: number;
  /** 成长的已完成 / 总数，如 "16/18" */
  progressSummary: string;
  status: 'success' | 'failed';
  /** 人类可读说明：奖品、失败原因、待手动完成项等 */
  message: string;
}

/** 一轮执行的日志记录 */
export interface WorkbuddyAutoTasksLogRecord {
  id: string;
  timestamp: string;
  date: string;
  durationMs: number;
  totalAccounts: number;
  successCount: number;
  failedCount: number;
  status: 'success' | 'partial' | 'failed' | 'no_accounts';
  details: WorkbuddyAutoTasksAccountDetail[];
}

/** 手动触发一轮的返回结果 */
export type WorkbuddyAutoTasksCycleResult =
  | 'disabled'
  | 'waiting'
  | 'already_ran_today'
  | 'completed'
  | 'retry'
  | 'already_running';

export const WORKBUDDY_AUTO_TASKS_CONFIG_CHANGED_EVENT =
  'workbuddy-auto-tasks-config-changed';
export const WORKBUDDY_AUTO_TASKS_LOGS_CHANGED_EVENT = 'workbuddy-auto-tasks-logs-changed';

/** 内存缓存：供面板首次渲染时同步取值，避免闪烁 */
let cachedConfig: WorkbuddyAutoTasksConfig | null = null;

/** 读取自动任务配置（失败时回退缓存或默认值） */
export async function getWorkbuddyAutoTasksConfigAsync(): Promise<WorkbuddyAutoTasksConfig> {
  try {
    const config = await invoke<WorkbuddyAutoTasksConfig>('get_workbuddy_auto_tasks_config');
    cachedConfig = config;
    return config;
  } catch (err) {
    console.warn('[WorkbuddyAutoTasks] 读取配置失败，使用缓存或默认值:', err);
    return cachedConfig ?? DEFAULT_WORKBUDDY_AUTO_TASKS_CONFIG;
  }
}

/** 同步读取缓存配置（未加载过时返回默认值） */
export function getWorkbuddyAutoTasksConfig(): WorkbuddyAutoTasksConfig {
  return cachedConfig ?? DEFAULT_WORKBUDDY_AUTO_TASKS_CONFIG;
}

/** 保存配置到 Rust 后端并同步缓存 */
export async function saveWorkbuddyAutoTasksConfigAsync(
  config: WorkbuddyAutoTasksConfig,
): Promise<void> {
  await invoke('save_workbuddy_auto_tasks_config', { config });
  cachedConfig = config;
}

/** 读取自动任务运行日志（近 30 天） */
export async function getWorkbuddyAutoTasksLogsAsync(): Promise<WorkbuddyAutoTasksLogRecord[]> {
  return await invoke<WorkbuddyAutoTasksLogRecord[]>('get_workbuddy_auto_tasks_logs');
}

/** 清空自动任务运行日志 */
export async function clearWorkbuddyAutoTasksLogs(): Promise<void> {
  await invoke('clear_workbuddy_auto_tasks_logs');
}

/** 手动触发一轮自动任务（force=true 时忽略执行窗口与今日已跑判断） */
export async function runWorkbuddyAutoTasksCycleIfNeeded(
  force = false,
): Promise<WorkbuddyAutoTasksCycleResult> {
  const res = await invoke<string>('run_workbuddy_auto_tasks_now', { force });
  const allowed: WorkbuddyAutoTasksCycleResult[] = [
    'disabled',
    'waiting',
    'already_ran_today',
    'completed',
    'retry',
    'already_running',
  ];
  return allowed.includes(res as WorkbuddyAutoTasksCycleResult)
    ? (res as WorkbuddyAutoTasksCycleResult)
    : 'completed';
}

/** 校验 "HH:mm" 格式 */
export function isValidTimeString(value: string): boolean {
  return /^([01]\d|2[0-3]):[0-5]\d$/.test(value);
}

/** 时间字符串转当日分钟数（"06:30" → 390） */
export function parseTimeToMinutes(timeStr: string): number {
  const parts = timeStr.split(':').map(Number);
  return (parts[0] ?? 0) * 60 + (parts[1] ?? 0);
}

/** 本地日期字符串（YYYY-MM-DD） */
export function getWorkbuddyAutoTasksTodayStr(): string {
  const now = new Date();
  const month = String(now.getMonth() + 1).padStart(2, '0');
  const day = String(now.getDate()).padStart(2, '0');
  return `${now.getFullYear()}-${month}-${day}`;
}
