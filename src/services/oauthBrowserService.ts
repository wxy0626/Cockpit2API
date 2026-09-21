import { invoke } from '@tauri-apps/api/core';

/** 所有平台 OAuth 都使用 Chrome 可信用户配置。 */
export async function openOAuthUrlInChromeIncognito(url: string): Promise<void> {
  await invoke('open_oauth_url_in_chrome_incognito', { url });
}
