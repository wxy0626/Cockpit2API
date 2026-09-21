import {
  WorkbuddyAccount,
  getWorkbuddyAccountDisplayEmail,
  getWorkbuddyPlanBadge,
  getWorkbuddyUsage,
} from '../types/workbuddy';
import * as workbuddyIntlService from '../services/workbuddyIntlService';
import { getProviderCurrentAccountId } from '../services/providerCurrentAccountService';
import { createProviderAccountStore } from './createProviderAccountStore';

/** 国际版账号缓存键与国内版分开，避免两边缓存互相覆盖 */
const WORKBUDDY_INTL_ACCOUNTS_CACHE_KEY = 'agtools.workbuddyIntl.accounts.cache';
const WORKBUDDY_INTL_CURRENT_ACCOUNT_ID_KEY = 'agtools.workbuddyIntl.current_account_id';

/**
 * WorkBuddy 国际版账号 store
 * 复用通用账号 store 工厂，仅换掉服务函数与平台标识。
 */
export const useWorkbuddyIntlAccountStore = createProviderAccountStore<WorkbuddyAccount>(
  WORKBUDDY_INTL_ACCOUNTS_CACHE_KEY,
  {
    listAccounts: workbuddyIntlService.listWorkbuddyIntlAccounts,
    deleteAccount: workbuddyIntlService.deleteWorkbuddyIntlAccount,
    deleteAccounts: workbuddyIntlService.deleteWorkbuddyIntlAccounts,
    injectAccount: workbuddyIntlService.injectWorkbuddyIntlToVSCode,
    refreshToken: workbuddyIntlService.refreshWorkbuddyIntlToken,
    refreshAllTokens: workbuddyIntlService.refreshAllWorkbuddyIntlTokens,
    importFromJson: workbuddyIntlService.importWorkbuddyIntlFromJson,
    exportAccounts: workbuddyIntlService.exportWorkbuddyIntlAccounts,
    updateAccountTags: workbuddyIntlService.updateWorkbuddyIntlAccountTags,
  },
  {
    getDisplayEmail: getWorkbuddyAccountDisplayEmail,
    getPlanBadge: getWorkbuddyPlanBadge,
    getUsage: getWorkbuddyUsage,
  },
  {
    platformId: 'workbuddy_intl',
    currentAccountIdKey: WORKBUDDY_INTL_CURRENT_ACCOUNT_ID_KEY,
    resolveCurrentAccountId: () => getProviderCurrentAccountId('workbuddy_intl'),
  },
);
