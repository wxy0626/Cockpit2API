import { QoderAccount, getQoderAccountDisplayEmail, getQoderPlanBadge, getQoderUsage } from '../types/qoder';
import * as qoderWorkIntlService from '../services/qoderWorkIntlService';
import { getProviderCurrentAccountId } from '../services/providerCurrentAccountService';
import { createProviderAccountStore } from './createProviderAccountStore';

/** QoderWork 国际版账号缓存独立存放，避免覆盖 Qoder CLI 账号缓存。 */
const QODERWORK_ACCOUNTS_CACHE_KEY = 'agtools.qoderwork.accounts.cache';
const QODERWORK_CURRENT_ACCOUNT_ID_KEY = 'agtools.qoderwork.current_account_id';

export const useQoderWorkIntlAccountStore = createProviderAccountStore<QoderAccount>(
  QODERWORK_ACCOUNTS_CACHE_KEY,
  {
    listAccounts: qoderWorkIntlService.listQoderWorkAccounts,
    deleteAccount: qoderWorkIntlService.deleteQoderWorkAccount,
    deleteAccounts: qoderWorkIntlService.deleteQoderWorkAccounts,
    injectAccount: qoderWorkIntlService.injectQoderWorkAccount,
    refreshToken: qoderWorkIntlService.refreshQoderWorkToken,
    refreshAllTokens: qoderWorkIntlService.refreshAllQoderWorkTokens,
    importFromJson: qoderWorkIntlService.importQoderWorkFromJson,
    exportAccounts: qoderWorkIntlService.exportQoderWorkAccounts,
    updateAccountTags: qoderWorkIntlService.updateQoderWorkAccountTags,
  },
  {
    getDisplayEmail: getQoderAccountDisplayEmail,
    getPlanBadge: getQoderPlanBadge,
    getUsage: getQoderUsage,
  },
  {
    platformId: 'qoderwork_intl',
    currentAccountIdKey: QODERWORK_CURRENT_ACCOUNT_ID_KEY,
    resolveCurrentAccountId: () => getProviderCurrentAccountId('qoderwork_intl'),
  },
);
