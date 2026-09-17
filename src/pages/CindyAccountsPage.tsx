/**
 * CindyAccountsPage —— Cindy 平台页。
 *
 * 结构照 ZCode 页：顶部页签栏 + 「账号总览 / API 网关」两个页签。
 *   - 账号总览：CindyAccountsView（复刻 ZCode 的卡片视图，完全独立实现）
 *   - API 网关：CindyGatewayPanel（OpenAI 兼容网关的接入信息与测试发送）
 *
 * 为什么不用共享的 `PlatformOverviewTabsHeader`：它的 `platform` 类型是写死的
 * `PlatformOverviewHeaderId` 联合（16 个平台，不含 cindy），加成员就得改共享文件。
 * 这里用 Cindy 自己的页签栏，零共享文件改动。
 *
 * 账号数据来自 sidecars/cindy2api（本机登录态自动发现 + OAuth 授权添加），
 * 前端不接触任何上游凭据 —— 拿到的只是脱敏后的账号状态。
 */
import { useState, type ReactNode } from 'react';
import { Cable, LayoutGrid } from 'lucide-react';

import { CindyAccountsView } from '../components/cindy/CindyAccountsView';
import { CindyGatewayPanel } from '../components/cindy/CindyGatewayPanel';

type CindyTab = 'overview' | 'gateway';

const TABS: Array<{ key: CindyTab; label: string; icon: ReactNode }> = [
  { key: 'overview', label: '账号总览', icon: <LayoutGrid size={15} /> },
  { key: 'gateway', label: 'API 网关', icon: <Cable size={15} /> },
];

export function CindyAccountsPage() {
  const [activeTab, setActiveTab] = useState<CindyTab>('overview');

  return (
    <div className="ghcp-accounts-page">
      <div className="cindy-tabs">
        {TABS.map((tab) => (
          <button
            key={tab.key}
            className={activeTab === tab.key ? 'active' : ''}
            onClick={() => setActiveTab(tab.key)}
          >
            {tab.icon}
            {tab.label}
          </button>
        ))}
      </div>

      {activeTab === 'gateway' ? <CindyGatewayPanel /> : <CindyAccountsView />}
    </div>
  );
}
