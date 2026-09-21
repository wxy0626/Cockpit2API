import { invoke } from '@tauri-apps/api/core';
import type { WorkbuddyAccount } from '../types/workbuddy';
import type { CheckinResponse, CheckinStatusResponse } from '../types/codebuddy';

export interface WorkbuddyOAuthLoginStartResponse {
  loginId: string;
  verificationUri: string;
  verificationUriComplete?: string | null;
  expiresIn: number;
  intervalSeconds: number;
}

export async function listWorkbuddyAccounts(): Promise<WorkbuddyAccount[]> {
  return await invoke('list_workbuddy_accounts');
}

export async function deleteWorkbuddyAccount(accountId: string): Promise<void> {
  return await invoke('delete_workbuddy_account', { accountId });
}

export async function deleteWorkbuddyAccounts(accountIds: string[]): Promise<void> {
  return await invoke('delete_workbuddy_accounts', { accountIds });
}

export async function importWorkbuddyFromJson(jsonContent: string): Promise<WorkbuddyAccount[]> {
  return await invoke('import_workbuddy_from_json', { jsonContent });
}

export async function importWorkbuddyFromLocal(): Promise<WorkbuddyAccount[]> {
  return await invoke('import_workbuddy_from_local');
}

export async function exportWorkbuddyAccounts(accountIds: string[]): Promise<string> {
  return await invoke('export_workbuddy_accounts', { accountIds });
}

export async function refreshWorkbuddyToken(accountId: string): Promise<WorkbuddyAccount> {
  return await invoke('refresh_workbuddy_token', { accountId });
}

export async function refreshAllWorkbuddyTokens(): Promise<number> {
  return await invoke('refresh_all_workbuddy_tokens');
}

export async function startWorkbuddyOAuthLogin(): Promise<WorkbuddyOAuthLoginStartResponse> {
  return await invoke('workbuddy_oauth_login_start');
}

/**
 * 打开 WorkBuddy 内置授权窗口（**始终使用隔离会话**）。
 *
 * ⚠️ 这里刻意固定 `incognito: true`，不接受调用方传入的开关。
 * WorkBuddy 的授权页会复用窗口里已有的登录态：一旦复用，第二次点「授权」就会
 * 立刻把上一个账号再写一遍（用户实测的「再次授权还是之前的账号」）。
 * 而添加账号这个场景每次都需要一个干净会话，所以统一走隔离窗口。
 *
 * 国内版 / 国际版共用同一个 Rust 命令：区域由授权地址里的 state 反查待处理登录得到。
 */
export async function openWorkbuddyOAuthWindow(authUrl: string): Promise<void> {
  await invoke('workbuddy_oauth_open_window', { authUrl, incognito: true });
}

export async function completeWorkbuddyOAuthLogin(loginId: string): Promise<WorkbuddyAccount> {
  return await invoke('workbuddy_oauth_login_complete', { loginId });
}

export async function cancelWorkbuddyOAuthLogin(loginId?: string): Promise<void> {
  return await invoke('workbuddy_oauth_login_cancel', { loginId: loginId ?? null });
}

export async function addWorkbuddyAccountWithToken(accessToken: string): Promise<WorkbuddyAccount> {
  return await invoke('add_workbuddy_account_with_token', { accessToken });
}

export async function updateWorkbuddyAccountTags(accountId: string, tags: string[]): Promise<WorkbuddyAccount> {
  return await invoke('update_workbuddy_account_tags', { accountId, tags });
}

export async function getWorkbuddyAccountsIndexPath(): Promise<string> {
  return await invoke('get_workbuddy_accounts_index_path');
}

export async function injectWorkbuddyToVSCode(accountId: string): Promise<string> {
  return await invoke('inject_workbuddy_to_vscode', { accountId });
}

export async function checkinWorkbuddy(accountId: string): Promise<CheckinResponse> {
  return await invoke('checkin_workbuddy', { accountId });
}

export async function getCheckinStatusWorkbuddy(accountId: string): Promise<CheckinStatusResponse> {
  return await invoke('get_checkin_status_workbuddy', { accountId });
}

export interface WorkviewSessionInfo {
  id: string;
  accountId: string;
  email: string;
  webviewLabel: string;
  consoleUrl: string;
  startedAt: number;
}

export async function isWorkbuddyWebviewSupported(): Promise<boolean> {
  return await invoke('is_workbuddy_webview_supported');
}

export async function openWorkbuddyWebview(accountId: string): Promise<WorkviewSessionInfo> {
  return await invoke('open_workbuddy_webview', { accountId });
}

export async function closeWorkbuddyWebview(accountId: string): Promise<void> {
  return await invoke('close_workbuddy_webview', { accountId });
}

export async function listWorkbuddyWebviewSessions(): Promise<WorkviewSessionInfo[]> {
  return await invoke('list_workbuddy_webview_sessions');
}
