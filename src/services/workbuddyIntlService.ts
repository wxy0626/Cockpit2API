import { invoke } from '@tauri-apps/api/core';
import type { WorkbuddyAccount } from '../types/workbuddy';

/**
 * WorkBuddy 国际版服务层
 *
 * 与国内版（workbuddyService）一一对应，命令名带 `_intl` 后缀。
 * 底层共用同一套 Rust 实现，差异只在 realm 配置：
 * 网关 = www.workbuddy.ai、账号目录 = workbuddy_intl_accounts。
 *
 * ⚠️ 国际版没有签到 / 成长任务 / 猫猫旅行，也不支持 VS Code 注入，
 *    相关能力在此不提供（对应入口会给出明确提示，而非静默失败）。
 */

export interface WorkbuddyIntlOAuthLoginStartResponse {
  loginId: string;
  verificationUri: string;
  verificationUriComplete?: string | null;
  expiresIn: number;
  intervalSeconds: number;
}

export async function listWorkbuddyIntlAccounts(): Promise<WorkbuddyAccount[]> {
  return await invoke('list_workbuddy_intl_accounts');
}

export async function deleteWorkbuddyIntlAccount(accountId: string): Promise<void> {
  return await invoke('delete_workbuddy_intl_account', { accountId });
}

export async function deleteWorkbuddyIntlAccounts(accountIds: string[]): Promise<void> {
  return await invoke('delete_workbuddy_intl_accounts', { accountIds });
}

export async function importWorkbuddyIntlFromJson(jsonContent: string): Promise<WorkbuddyAccount[]> {
  return await invoke('import_workbuddy_intl_from_json', { jsonContent });
}

export async function exportWorkbuddyIntlAccounts(accountIds: string[]): Promise<string> {
  return await invoke('export_workbuddy_intl_accounts', { accountIds });
}

export async function refreshWorkbuddyIntlToken(accountId: string): Promise<WorkbuddyAccount> {
  return await invoke('refresh_workbuddy_intl_token', { accountId });
}

export async function refreshAllWorkbuddyIntlTokens(): Promise<number> {
  return await invoke('refresh_all_workbuddy_intl_tokens');
}

export async function startWorkbuddyIntlOAuthLogin(): Promise<WorkbuddyIntlOAuthLoginStartResponse> {
  return await invoke('workbuddy_intl_oauth_login_start');
}

/**
 * 打开 WorkBuddy 国际版内置授权窗口（**始终使用隔离会话**）。
 *
 * ⚠️ 与国内版同因：授权页会复用已有登录态，复用就会「再次授权还是之前的账号」，
 * 所以固定 `incognito: true`。
 *
 * 注意命令名没有 `_intl` 后缀 —— 开窗区域由授权地址里的 state 反查待处理登录得到，
 * 两个区域共用同一个命令，这不是遗漏。
 */
export async function openWorkbuddyIntlOAuthWindow(authUrl: string): Promise<void> {
  await invoke('workbuddy_oauth_open_window', { authUrl, incognito: true });
}

export async function completeWorkbuddyIntlOAuthLogin(loginId: string): Promise<WorkbuddyAccount> {
  return await invoke('workbuddy_intl_oauth_login_complete', { loginId });
}

export async function cancelWorkbuddyIntlOAuthLogin(loginId?: string): Promise<void> {
  return await invoke('workbuddy_intl_oauth_login_cancel', { loginId: loginId ?? null });
}

export async function addWorkbuddyIntlAccountWithToken(accessToken: string): Promise<WorkbuddyAccount> {
  return await invoke('add_workbuddy_intl_account_with_token', { accessToken });
}

export async function updateWorkbuddyIntlAccountTags(
  accountId: string,
  tags: string[],
): Promise<WorkbuddyAccount> {
  return await invoke('update_workbuddy_intl_account_tags', { accountId, tags });
}

export async function getWorkbuddyIntlAccountsIndexPath(): Promise<string> {
  return await invoke('get_workbuddy_intl_accounts_index_path');
}

/** 国际版切换的是内部当前账号，不写外部客户端凭据。 */
export async function injectWorkbuddyIntlToVSCode(accountId: string): Promise<string> {
  await invoke('switch_workbuddy_intl_account', { accountId });
  return accountId;
}
