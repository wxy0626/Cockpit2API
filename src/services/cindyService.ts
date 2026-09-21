/**
 * cindyService.ts —— Cindy 平台的数据服务。
 *
 * 与其它平台的 dataService 不同：这里**不是 Tauri invoke，而是 HTTP**——
 * 账号由 sidecars/cindy2api 提供（本机登录态自动发现 + OAuth 授权添加）。
 * 所以本文件是 sidecar 管理接口的薄客户端，不做任何本地持久化。
 *
 * ⚠️ 唯一两处例外是授权窗口的开关（`openOAuthWindow` / `closeOAuthWindow`）：
 * 那是 Tauri 命令，因为授权窗口由宿主统一创建，sidecar 自身拉起的是系统浏览器。
 *
 * 语义映射提醒：
 *   - 「刷新 token」在 Cindy 语境下没有对应动作（凭据由 Cindy 客户端或 OAuth 维护），
 *     统一映射为「重新探测该账号」，不要凭空造 token 刷新。
 *   - 「删除」只允许删除 `oauth-` 前缀的授权账号；本机账号属于 Cindy 客户端，
 *     删除本地凭据会破坏用户登录态，sidecar 侧会明确拒绝。
 */
import { invoke } from '@tauri-apps/api/core';
import {
  toCindyAccount,
  type CindyAccount,
  type CindyAccountView,
} from '../types/cindy';

/** sidecar 默认监听地址，与 sidecars/cindy2api/runtime/config.json 的 listen 对应 */
export const CINDY_GATEWAY_BASE = 'http://127.0.0.1:7865';

/** 网关不可达时给出的统一提示（用户最常遇到的失败就是 sidecar 没起来） */
const UNREACHABLE_HINT =
  '无法连接 Cindy 网关服务。请确认 Cockpit2API 已正常启动（网关由它托管拉起），' +
  '或手动执行 sidecars/cindy2api/bin/cindy2api.exe。';

/** 统一的请求封装：把 HTTP 失败与错误体转成中文可读的错误 */
async function call<T>(path: string, init?: RequestInit): Promise<T> {
  let response: Response;
  try {
    response = await fetch(`${CINDY_GATEWAY_BASE}${path}`, {
      ...init,
      headers: {
        'Content-Type': 'application/json',
        ...(init?.headers ?? {}),
      },
    });
  } catch (error) {
    throw new Error(`${UNREACHABLE_HINT}（${error instanceof Error ? error.message : String(error)}）`);
  }

  const text = await response.text();
  let payload: unknown = null;
  if (text) {
    try {
      payload = JSON.parse(text);
    } catch {
      payload = null;
    }
  }

  if (!response.ok) {
    const message =
      (payload as { error?: { message?: string } } | null)?.error?.message ??
      `请求失败（HTTP ${response.status}）`;
    throw new Error(message);
  }
  return payload as T;
}

/** 账号池快照 */
interface AccountsResponse {
  total: number;
  available: number;
  accounts: CindyAccountView[];
  models: string[];
}

/** 拉取全部账号（两个来源合并后由 sidecar 返回） */
export async function listAccounts(): Promise<CindyAccount[]> {
  const data = await call<AccountsResponse>('/api/accounts');
  return (data.accounts ?? []).map(toCindyAccount);
}

/** 触发 sidecar 重新扫描本机 Cindy 登录态，并返回新增数量 */
export async function importLocalAccounts(): Promise<{ total: number; added: number }> {
  const data = await call<{ total?: number; added?: number }>('/api/refresh', { method: 'POST' });
  return { total: data.total ?? 0, added: data.added ?? 0 };
}

/** 触发一次全量健康检查（对应视图上的「刷新」按钮） */
export async function checkAllAccounts(): Promise<CindyAccount[]> {
  const data = await call<{ accounts?: CindyAccountView[] }>('/api/check', { method: 'POST' });
  return (data.accounts ?? []).map(toCindyAccount);
}

/**
 * 重新探测单个账号。
 *
 * 注意语义：这不是「刷新 token」。sidecar 的巡检是全量的，
 * 这里调用后返回全量结果再由调用方筛选，避免为单账号再加一个端点。
 */
export async function refreshAccount(id: string): Promise<CindyAccount | null> {
  const accounts = await checkAllAccounts();
  return accounts.find((account) => account.id === id) ?? null;
}

/** 单个账号的额度（金额是字符串，保留服务端精度，展示时再格式化） */
export interface CreditInfo {
  accountId: string;
  /** cn = 中国大陆版（人民币计价）；global = 国际版（美元计价） */
  region: string;
  /** 总可用 */
  available?: string;
  /** 全部桶的总额度（进度条分母） */
  total?: string;
  /** 已用（进度条分子） */
  used?: string;
  scale?: number;
  error?: string;
}

/**
 * 拉取全部手动添加账号的额度。
 *
 * 注意两点：
 *   1. 只覆盖「授权 / 手机号登录」添加的账号 —— 本机账号手里只有网关 key，
 *      额度接口只认登录令牌，它们没有 refreshToken 可换，查不了。
 *   2. sidecar 侧每次查询都会轮换 refreshToken 并自行持久化。
 */
export async function fetchCredits(): Promise<Record<string, CreditInfo>> {
  const data = await call<{ credits?: CreditInfo[] }>('/api/credits', { method: 'POST' });
  const map: Record<string, CreditInfo> = {};
  for (const item of data.credits ?? []) {
    map[item.accountId] = item;
  }
  return map;
}

/**
 * 移除账号。
 *
 * 两类账号的移除方式不同，按 id 前缀分流：
 *   - `oauth-*`：本工具授权添加的，凭据归我们所有 → 删除我们自己的记录（/api/accounts/remove）
 *   - 其它（本机发现的）：删掉 Cindy 客户端的凭据文件（/api/accounts/delete-local）
 *
 * 第二种等价于「该账号在 Cindy 客户端登出」，下次需重新登录 —— 这是明确的设计选择：
 * 平台页的账号卡片负责多账号切换，不再依赖客户端登录态。
 *
 * sidecar 侧仍守住一条红线：只删该账号专属的 `owner_<id>_*.enc`，
 * **绝不碰共用的 Local State**（OSCrypt 主密钥，删了整个 profile 的账号都会失效）。
 */
export async function removeAccounts(ids: string[]): Promise<void> {
  for (const id of ids) {
    const isOauthAccount = id.startsWith('oauth-');
    await call(isOauthAccount ? '/api/accounts/remove' : '/api/accounts/delete-local', {
      method: 'POST',
      body: JSON.stringify({ accountId: id }),
    });
  }
}

/** 更新账号标签 —— Cindy 侧暂无标签存储，按无操作处理并说明原因 */
export async function updateAccountTags(): Promise<void> {
  throw new Error('Cindy 账号暂不支持自定义标签。');
}

/** 某区域支持的登录方式 */
export interface CindyProviders {
  region?: string;
  email?: boolean;
  phone?: boolean;
  social?: string[];
}

export async function getProviders(region: 'global' | 'cn'): Promise<CindyProviders> {
  return call<CindyProviders>(`/api/login/providers?region=${region}`);
}

/**
 * 发起 OAuth 授权：返回会话 id 与授权地址。
 *
 * `openBrowser: false` 时不拉起系统浏览器 —— 由调用方用可信授权窗口打开
 * （Cockpit2API 走这条，避免复用系统浏览器登录态导致「再次授权还是上一个账号」）。
 */
export interface OAuthStartResult {
  sessionId: string;
  authorizeUrl: string;
  provider: string;
  region: string;
}

export async function startOAuth(
  provider: string,
  region: 'global' | 'cn',
  openBrowser = true,
): Promise<OAuthStartResult> {
  return call<OAuthStartResult>('/api/login/oauth/start', {
    method: 'POST',
    body: JSON.stringify({ provider, region, openBrowser }),
  });
}

/** 打开可信授权窗口（区域取自授权地址，Rust 侧按授权域校验）。 */
export async function openOAuthWindow(authorizeUrl: string): Promise<void> {
  return await invoke('cindy_oauth_window_open', { authorizeUrl });
}

/** 关闭应用内授权窗口（用户中途放弃时调用）。 */
export async function closeOAuthWindow(): Promise<void> {
  return await invoke('cindy_oauth_window_close');
}

/** 轮询授权结果；status 为 ok 时账号已由 sidecar 落库 */
export interface OAuthPollResult {
  status: 'pending' | 'ok';
  label?: string;
  endpoint?: string;
}

export async function pollOAuth(sessionId: string): Promise<OAuthPollResult> {
  return call<OAuthPollResult>('/api/login/oauth/poll', {
    method: 'POST',
    body: JSON.stringify({ sessionId }),
  });
}

/** 网关自身状态（用于卡片页顶部的连接提示） */
export async function getGatewayHealth(): Promise<{ accounts: string; status: string }> {
  return call<{ accounts: string; status: string }>('/health');
}
