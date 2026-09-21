/**
 * cindy.ts —— Cindy 平台的账号类型与展示映射。
 *
 * Cindy 与其它平台的**根本差异**：账号不在 Cockpit2API 的本地账号库里，
 * 而是由 sidecars/cindy2api 提供（本机登录态自动发现 + OAuth 授权添加）。
 * 因此这里只做「sidecar 返回值 → 通用账号视图所需形状」的映射，
 * 不涉及任何 Tauri 调用与本地持久化。
 */
import type { ProviderAccountBase } from '../hooks/useProviderAccountsPage';

/** 账号来源 */
export type CindyAccountSource = 'local' | 'oauth';

/** sidecar /api/accounts 返回的单个账号 */
export interface CindyAccountView {
  ownerId: string;
  keyMasked: string;
  endpoint: string;
  profiles: string[];
  subscriptions: string[];
  source: string;
  status: string;
  statusDetail: string;
  modelCount: number;
  latencyMs: number;
  lastCheckedAt: number;
}

/**
 * Cindy 账号（满足通用账号视图的最小契约）。
 *
 * `id` 直接用 sidecar 的 ownerId（本地账号是 owner 哈希、授权账号是 `oauth-` 前缀），
 * `created_at` 用首次探测时间 —— 视图的默认排序就是按它倒序。
 */
export interface CindyAccount extends ProviderAccountBase {
  id: string;
  created_at: number;
  tags?: string[] | null;

  /** 账号展示名：授权账号用 displayName/email，本机账号用 profile 名 */
  label: string;
  /** 推理入口（含 host，用于卡片副标题） */
  endpoint: string;
  /** 脱敏后的上游 key */
  apiKeyMasked: string;
  source: CindyAccountSource;
  /** ok / error / unknown */
  status: string;
  /** 状态说明或具体错误（中文） */
  statusDetail: string;
  /** 该账号可用的模型数量 */
  modelCount: number;
  latencyMs: number;
  lastCheckedAt: number;
  /** 该账号携带的订阅凭证（claude-code / codex / pi），本机账号可能有 */
  subscriptions: string[];
}

/** 把 sidecar 返回的一行转成视图账号 */
export function toCindyAccount(view: CindyAccountView): CindyAccount {
  return {
    id: view.ownerId,
    // 首次探测时间缺失时用 0：排序会沉底，但不会让卡片消失
    created_at: view.lastCheckedAt || 0,
    tags: null,
    label: view.profiles?.[0] || view.endpoint.replace(/^https?:\/\//, ''),
    endpoint: view.endpoint,
    apiKeyMasked: view.keyMasked,
    source: view.source === 'oauth' ? 'oauth' : 'local',
    status: view.status || 'unknown',
    statusDetail: view.statusDetail || '',
    modelCount: view.modelCount || 0,
    latencyMs: view.latencyMs || 0,
    lastCheckedAt: view.lastCheckedAt || 0,
    subscriptions: view.subscriptions ?? [],
  };
}

/** 卡片主标题 */
export function getCindyDisplayName(account: CindyAccount): string {
  return account.label || account.endpoint;
}

/** 卡片副标题（脱敏 key + 入口 host） */
export function getCindySubtitle(account: CindyAccount): string {
  return `${account.apiKeyMasked} · ${account.endpoint.replace(/^https?:\/\//, '')}`;
}

/**
 * 卡片右上角的角标。
 *
 * Cindy 没有「套餐」概念（它不是订阅制平台），所以这里表达的是**账号来源**，
 * 与 ZCode 的 plan badge 位置一致但语义不同。
 */
export function getCindyPlanBadge(account: CindyAccount): string {
  return account.source === 'oauth' ? '授权添加' : '本机登录态';
}

/** 卡片上的状态文案（对齐 ZCode 「用量状态 正常」那一行） */
export function getCindyStatusText(account: CindyAccount): string {
  if (account.status === 'ok') return '可用';
  if (account.status === 'error') return '不可用';
  return '未探测';
}

/** 状态是否正常（决定文字颜色） */
export function isCindyAccountHealthy(account: CindyAccount): boolean {
  return account.status === 'ok';
}

/**
 * 错误详情在卡片上只显示首句。
 *
 * 完整的排查建议（如 TLS 握手中断那段）有一两百字，直接铺在卡片里会撑破布局、
 * 把时间戳都挤变形。完整内容通过 `title` 提示给出，卡片上只看结论。
 */
export function summarizeCindyDetail(detail: string): string {
  const text = (detail ?? '').trim();
  if (!text) return '尚未探测';
  const firstSentence = text.split(/[。\n]/)[0] ?? text;
  return firstSentence.length > 46 ? `${firstSentence.slice(0, 46)}…` : firstSentence;
}

/**
 * 「配额」区域的内容。
 *
 * Cindy 的可用额度由上游账号承担、网关侧拿不到配额数字，因此这里展示
 * 该账号能用的模型数量；视图层用 `hasCindyQuotaData` 决定是否显示占位文案。
 */
export function getCindyQuotaGroups(account: CindyAccount): Array<{ label: string; value: string }> {
  if (account.modelCount <= 0) return [];
  return [{ label: '可用模型', value: String(account.modelCount) }];
}

/** 是否有可展示的额度信息 */
export function hasCindyQuotaData(account: CindyAccount): boolean {
  return account.modelCount > 0;
}

/** 用量补充信息（延迟），显示在卡片底部；失败时给首句原因 */
export function getCindyUsage(account: CindyAccount): string {
  if (account.status !== 'ok') return summarizeCindyDetail(account.statusDetail);
  return account.latencyMs > 0 ? `延迟 ${account.latencyMs} ms` : '延迟 —';
}
