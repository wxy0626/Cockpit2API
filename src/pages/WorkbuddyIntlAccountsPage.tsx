import { useMemo } from 'react';
import { useWorkbuddyIntlAccountStore } from '../stores/useWorkbuddyIntlAccountStore';
import * as workbuddyIntlService from '../services/workbuddyIntlService';
import {
  WorkbuddyAccount,
  getWorkbuddyAccountDisplayEmail,
  getWorkbuddyPlanBadge,
  getWorkbuddyUsage,
  getWorkbuddyQuotaCategoryGroups,
} from '../types/workbuddy';
import { useProviderAccountsPage } from '../hooks/useProviderAccountsPage';
import { PlatformOverviewTabsHeader } from '../components/platform/PlatformOverviewTabsHeader';
import {
  CodebuddySuiteAccountsSharedView,
  type CodebuddySuiteAccountsPlatformConfig,
} from '../components/codebuddy-suite/CodebuddySuiteAccountsSharedView';
import { WbCardLimitInput, useWbPoolLimits } from '../components/codebuddy-suite/WorkbuddyGatewayConcurrency';

/**
 * WorkBuddy 国际版账号页
 *
 * 与国内版（WorkbuddyAccountsPage）共用同一套共享视图与通用账号页 hook，
 * 差异仅在于：store / service 走国际版命令（realm=intl）。
 *
 * ⚠️ 国际版没有签到、成长任务、猫猫旅行与 VS Code 注入，
 *    因此这里不挂载签到弹窗，注入入口会返回明确提示而非静默失败。
 */

const WORKBUDDY_INTL_FLOW_NOTICE_COLLAPSED_KEY = 'agtools.workbuddyIntl.flow_notice_collapsed';
const WORKBUDDY_INTL_CURRENT_ACCOUNT_ID_KEY = 'agtools.workbuddyIntl.current_account_id';

const workbuddyIntlPlatformConfig: CodebuddySuiteAccountsPlatformConfig<WorkbuddyAccount> = {
  pageClassName: 'workbuddy-intl-accounts-page',
  searchPlaceholderKey: 'workbuddyIntl.search',
  searchPlaceholderDefault: '搜索 WorkBuddy 国际版账号...',
  flowNotice: {
    titleKey: 'workbuddyIntl.flowNotice.title',
    titleDefault: 'WorkBuddy 国际版账号管理说明（点击展开/收起）',
    descKey: 'workbuddyIntl.flowNotice.desc',
    descDefault:
      '账号凭证加密保存在本机，Token 刷新与授权均直连 WorkBuddy 国际版（www.workbuddy.ai），数据仅在本地处理。',
    permissionKey: 'workbuddyIntl.flowNotice.permission',
    permissionDefault:
      '权限范围：读取本机 WorkBuddy 国际版账号存储，调用系统凭据能力（macOS Keychain / Windows DPAPI / Linux Secret Service）进行解密与回写。',
    networkKey: 'workbuddyIntl.flowNotice.network',
    networkDefault:
      '网络范围：OAuth 授权登录与 Token 刷新需联网请求 www.workbuddy.ai。不上传本地密钥或凭证。',
  },
  noAccountsKey: 'workbuddyIntl.noAccounts',
  noAccountsDefault: '暂无 WorkBuddy 国际版账号',
  addAccountTitleKey: 'workbuddyIntl.addAccount',
  addAccountTitleDefault: '添加 WorkBuddy 国际版账号',
  oauthDescKey: 'workbuddyIntl.oauthDesc',
  oauthDescDefault: '点击下方按钮将在独立授权窗口中打开 WorkBuddy 国际版授权页面（支持 Google / GitHub 登录）。',
  oauthOpenButtonKey: 'common.shared.oauth.openAuthWindow',
  oauthOpenButtonDefault: '打开授权窗口',
  oauthFeatureCardClassName: 'workbuddy-oauth-feature-card',
  oauthFeatureTitleKey: 'workbuddyIntl.oauthFeature.oauth.title',
  oauthFeatureTitleDefault: '浏览器授权，无需安装客户端',
  oauthFeatureItem1Key: 'workbuddyIntl.oauthFeature.oauth.item1',
  oauthFeatureItem1Default: '在浏览器完成 OAuth 后即可添加账号。',
  oauthFeatureItem2Key: 'workbuddyIntl.oauthFeature.oauth.item2',
  oauthFeatureItem2Default: '授权完成后会自动刷新一次账号信息。',
  oauthFeatureItem3Key: 'workbuddyIntl.oauthFeature.oauth.item3',
  oauthFeatureItem3Default: '国际版账号与国内版账号分开存放，互不干扰。',
  oauthUrlInputPlaceholderKey: 'workbuddyIntl.oauthUrlInputPlaceholder',
  oauthUrlInputPlaceholderDefault: '可手动输入授权地址',
  oauthWaitingKey: 'workbuddyIntl.oauthWaiting',
  oauthWaitingDefault: '等待授权完成...',
  tokenDescKey: 'workbuddyIntl.tokenDesc',
  tokenDescDefault: '粘贴 WorkBuddy 国际版的 access token：',
  importLocalDescKey: 'workbuddyIntl.import.localDesc',
  importLocalDescDefault: '支持从 JSON 文件导入国际版账号数据。',
  importLocalClientKey: 'workbuddyIntl.import.localClient',
  importLocalClientDefault: '从本机导入',
  getDisplayEmail: (account) => getWorkbuddyAccountDisplayEmail(account),
  getPlanBadge: (account) => getWorkbuddyPlanBadge(account),
  getUsage: (account) => getWorkbuddyUsage(account),
  getQuotaGroups: (account, t) => getWorkbuddyQuotaCategoryGroups(account, t),
  hasQuotaData: (_account, groups) => groups.some((g) => g.items.length > 0),
  usagePrefix: 'workbuddy',
  quotaPrefix: 'workbuddy',
  tableUsageClassName: 'workbuddy-table-usage',
};

export function WorkbuddyIntlAccountsPage() {
  const store = useWorkbuddyIntlAccountStore();
  const poolLimits = useWbPoolLimits('intl');
  const platformConfig = useMemo(
    () => ({
      ...workbuddyIntlPlatformConfig,
      cardBadgesOnSecondRow: true,
      renderCardStatusBadges: (account: WorkbuddyAccount) => {
        const poolKey = account.uid || account.id;
        const pool = poolLimits.map[poolKey];
        return (
          <WbCardLimitInput
            uid={poolKey}
            value={pool?.max_in_flight ?? 1}
            inFlight={pool?.in_flight ?? 0}
            onSaved={() => void poolLimits.reload()}
          />
        );
      },
    }),
    [poolLimits.map, poolLimits.reload],
  );

  const page = useProviderAccountsPage<WorkbuddyAccount>({
    platformKey: 'WorkBuddy Intl',
    oauthLogPrefix: 'WorkbuddyIntlOAuth',
    flowNoticeCollapsedKey: WORKBUDDY_INTL_FLOW_NOTICE_COLLAPSED_KEY,
    currentAccountIdKey: WORKBUDDY_INTL_CURRENT_ACCOUNT_ID_KEY,
    exportFilePrefix: 'workbuddy_intl_accounts',
    oauthTabKeys: ['oauth'],
    store: {
      accounts: store.accounts,
      currentAccountId: store.currentAccountId,
      loading: store.loading,
      error: store.error,
      fetchAccounts: store.fetchAccounts,
      fetchCurrentAccountId: store.fetchCurrentAccountId,
      deleteAccounts: store.deleteAccounts,
      refreshToken: store.refreshToken,
      refreshAllTokens: store.refreshAllTokens,
      setCurrentAccountId: store.setCurrentAccountId,
      updateAccountTags: store.updateAccountTags,
    },
    oauthService: {
      startLogin: workbuddyIntlService.startWorkbuddyIntlOAuthLogin,
      completeLogin: workbuddyIntlService.completeWorkbuddyIntlOAuthLogin,
      cancelLogin: workbuddyIntlService.cancelWorkbuddyIntlOAuthLogin,
      // 内置授权窗口：走隔离会话，避免复用已有登录态导致再次授权还是同一个账号
      openAuthUrl: workbuddyIntlService.openWorkbuddyIntlOAuthWindow,
    },
    dataService: {
      importFromJson: workbuddyIntlService.importWorkbuddyIntlFromJson,
      addWithToken: workbuddyIntlService.addWorkbuddyIntlAccountWithToken,
      exportAccounts: workbuddyIntlService.exportWorkbuddyIntlAccounts,
      injectToVSCode: workbuddyIntlService.injectWorkbuddyIntlToVSCode,
    },
    getDisplayEmail: (account) => getWorkbuddyAccountDisplayEmail(account),
  });

  return (
    <div className={`ghcp-accounts-page ${workbuddyIntlPlatformConfig.pageClassName}`}>
      {/* 挂载页头：其内部分组切换器让用户能在 WorkBuddy 各版本间来回切换 */}
      <PlatformOverviewTabsHeader
        platform="workbuddy_intl"
        active="overview"
        tabs={['overview']}
      />
      <CodebuddySuiteAccountsSharedView
        accounts={store.accounts}
        loading={store.loading}
        page={page}
        platformConfig={platformConfig}
        onRefreshAccounts={() => {
          store.fetchAccounts();
        }}
      />
    </div>
  );
}
