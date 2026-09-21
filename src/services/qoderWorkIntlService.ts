import { invoke } from '@tauri-apps/api/core';
import type { QoderAccount } from '../types/qoder';

export interface QoderWorkOAuthStartResponse {
  loginId: string;
  verificationUri: string;
  expiresIn: number;
  intervalSeconds: number;
  callbackUrl?: string | null;
}

type QoderWorkOAuthStartResponseRaw = Partial<QoderWorkOAuthStartResponse> & {
  login_id?: string;
  verification_uri?: string;
  expires_in?: number;
  interval_seconds?: number;
  callback_url?: string | null;
};

function normalizeOAuthStartResponse(
  raw: QoderWorkOAuthStartResponseRaw,
): QoderWorkOAuthStartResponse {
  const loginId = raw.loginId ?? raw.login_id ?? '';
  const verificationUri = raw.verificationUri ?? raw.verification_uri ?? '';
  const expiresIn = Number(raw.expiresIn ?? raw.expires_in ?? 600);
  const intervalSeconds = Number(raw.intervalSeconds ?? raw.interval_seconds ?? 1);

  if (!loginId || !verificationUri) {
    throw new Error('QoderWork OAuth start 响应缺少关键字段');
  }

  return {
    loginId,
    verificationUri,
    expiresIn: Number.isFinite(expiresIn) && expiresIn > 0 ? expiresIn : 600,
    intervalSeconds: Number.isFinite(intervalSeconds) && intervalSeconds > 0 ? intervalSeconds : 1,
    callbackUrl: raw.callbackUrl ?? raw.callback_url ?? null,
  };
}

export async function listQoderWorkAccounts(): Promise<QoderAccount[]> {
  return await invoke('list_qoderwork_accounts');
}

export async function deleteQoderWorkAccount(accountId: string): Promise<void> {
  return await invoke('delete_qoderwork_account', { accountId });
}

export async function deleteQoderWorkAccounts(accountIds: string[]): Promise<void> {
  return await invoke('delete_qoderwork_accounts', { accountIds });
}

export async function importQoderWorkFromJson(jsonContent: string): Promise<QoderAccount[]> {
  return await invoke('import_qoderwork_from_json', { jsonContent });
}

export async function exportQoderWorkAccounts(accountIds: string[]): Promise<string> {
  return await invoke('export_qoderwork_accounts', { accountIds });
}

export async function startQoderWorkOAuthLogin(): Promise<QoderWorkOAuthStartResponse> {
  return normalizeOAuthStartResponse(await invoke('qoderwork_oauth_login_start'));
}

export async function completeQoderWorkOAuthLogin(loginId: string): Promise<QoderAccount> {
  return await invoke('qoderwork_oauth_login_complete', { loginId });
}

/** 现有 Qoder OAuth 是单槽位；start 超时后可从这里找回 pending 会话。 */
export async function peekQoderWorkOAuthLogin(): Promise<QoderWorkOAuthStartResponse | null> {
  return await invoke('qoder_oauth_login_peek');
}

export async function cancelQoderWorkOAuthLogin(loginId?: string): Promise<void> {
  return await invoke('qoderwork_oauth_login_cancel', { loginId: loginId ?? null });
}

/** 使用可信配置，复用本机登录态完成授权。 */
export async function openQoderWorkOAuthWindow(authUrl: string): Promise<void> {
  return await invoke('qoderwork_oauth_open_window', { authUrl });
}

export async function closeQoderWorkOAuthWindow(): Promise<void> {
  return await invoke('qoderwork_oauth_close_window');
}

export async function refreshQoderWorkToken(accountId: string): Promise<QoderAccount> {
  return await invoke('refresh_qoderwork_token', { accountId });
}

export async function refreshAllQoderWorkTokens(): Promise<number> {
  return await invoke('refresh_all_qoderwork_tokens');
}

export async function updateQoderWorkAccountTags(
  accountId: string,
  tags: string[],
): Promise<QoderAccount> {
  return await invoke('update_qoderwork_account_tags', { accountId, tags });
}

export async function getQoderWorkAccountsIndexPath(): Promise<string> {
  return await invoke('get_qoderwork_accounts_index_path');
}

/** QoderWork 页面切换的是内部当前账号，网关凭据会跟随该账号。 */
export async function injectQoderWorkAccount(accountId: string): Promise<unknown> {
  return await invoke('switch_qoderwork_intl_account', { accountId });
}
