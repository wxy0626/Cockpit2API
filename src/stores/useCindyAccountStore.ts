/**
 * useCindyAccountStore —— Cindy 平台的账号 store。
 *
 * 实现通用账号视图要求的 `ProviderStoreActions` 契约，但**数据源是 sidecar 的 HTTP 接口**
 * 而不是本地账号库（其它平台是 Tauri invoke）。因此这里刻意做了两件事：
 *
 *   1. 不假装有本地写能力：标签、切号注入等 Cindy 侧不存在的动作如实抛错或空转，
 *      而不是静默成功（静默成功会让用户以为设置生效了）。
 *   2. 刷新去重：视图可能在多个地方同时触发刷新，用 in-flight 标记拦住并发请求，
 *      避免把刚启动的 sidecar 打满。
 */
import { create } from 'zustand';

import * as cindyService from '../services/cindyService';
import type { CindyAccount } from '../types/cindy';

/** 当前选中账号的持久化 key（与视图的 currentAccountIdKey 对应） */
export const CINDY_CURRENT_ACCOUNT_KEY = 'agtools.cindy.current_account_id';

/** 读取持久化的当前账号 id */
function readStoredCurrentAccountId(): string | null {
  try {
    return window.localStorage.getItem(CINDY_CURRENT_ACCOUNT_KEY);
  } catch {
    return null;
  }
}

/** 写入当前账号 id（失败不影响功能） */
function writeStoredCurrentAccountId(accountId: string | null): void {
  try {
    if (accountId) {
      window.localStorage.setItem(CINDY_CURRENT_ACCOUNT_KEY, accountId);
    } else {
      window.localStorage.removeItem(CINDY_CURRENT_ACCOUNT_KEY);
    }
  } catch {
    /* localStorage 不可用时静默降级为内存态 */
  }
}

interface CindyAccountStoreState {
  accounts: CindyAccount[];
  currentAccountId: string | null;
  loading: boolean;
  error: string | null;

  fetchAccounts: () => Promise<void>;
  fetchCurrentAccountId: () => Promise<string | null>;
  setCurrentAccountId: (accountId: string | null) => void;
  /** 重新探测全部账号（视图的「刷新」按钮） */
  refreshAllTokens: () => Promise<void>;
  refreshToken: (id: string) => Promise<void>;
  deleteAccounts: (ids: string[]) => Promise<void>;
  /** 从本机 Cindy 登录态导入（视图的「导入」按钮） */
  importLocal: () => Promise<{ total: number; added: number }>;
  /** 授权成功后由对话框回调，用于立即刷新列表 */
  onAccountAdded: () => Promise<void>;
  switchAccount: (accountId: string) => Promise<void>;
  updateAccountTags: (id: string, tags: string[]) => Promise<void>;
}

/** 并发刷新闸门：同一时刻只允许一次拉取 */
let inflight: Promise<void> | null = null;

export const useCindyAccountStore = create<CindyAccountStoreState>((set, get) => ({
  accounts: [],
  currentAccountId: readStoredCurrentAccountId(),
  loading: false,
  error: null,

  fetchAccounts: async () => {
    if (inflight) return inflight;
    set({ loading: true, error: null });
    inflight = (async () => {
      try {
        const accounts = await cindyService.listAccounts();
        set({ accounts, loading: false, error: null });
        // 选中的账号被删掉时清理掉悬空的 currentAccountId
        const current = get().currentAccountId;
        if (current && !accounts.some((account) => account.id === current)) {
          writeStoredCurrentAccountId(null);
          set({ currentAccountId: null });
        }
      } catch (error) {
        set({
          loading: false,
          error: error instanceof Error ? error.message : String(error),
        });
      } finally {
        inflight = null;
      }
    })();
    return inflight;
  },

  fetchCurrentAccountId: async () => get().currentAccountId,

  setCurrentAccountId: (accountId) => {
    writeStoredCurrentAccountId(accountId);
    set({ currentAccountId: accountId });
  },

  refreshAllTokens: async () => {
    try {
      const accounts = await cindyService.checkAllAccounts();
      set({ accounts, error: null });
    } catch (error) {
      set({ error: error instanceof Error ? error.message : String(error) });
    }
  },

  refreshToken: async (id) => {
    try {
      await cindyService.refreshAccount(id);
      await get().fetchAccounts();
    } catch (error) {
      set({ error: error instanceof Error ? error.message : String(error) });
    }
  },

  deleteAccounts: async (ids) => {
    await cindyService.removeAccounts(ids);
    await get().fetchAccounts();
  },

  importLocal: async () => {
    const result = await cindyService.importLocalAccounts();
    await get().fetchAccounts();
    return result;
  },

  onAccountAdded: async () => {
    await get().fetchAccounts();
  },

  /**
   * 选择当前账号。
   *
   * Cindy 没有「切号注入」这个动作（账号不需要写进任何第三方客户端配置），
   * 所以这里只记录选择结果，不做任何文件写入 —— 与其它平台的 switchAccount 语义不同。
   */
  switchAccount: async (accountId) => {
    get().setCurrentAccountId(accountId);
  },

  updateAccountTags: async () => {
    throw new Error('Cindy 账号暂不支持自定义标签。');
  },
}));
