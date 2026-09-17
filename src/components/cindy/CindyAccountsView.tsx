/**
 * CindyAccountsView —— Cindy 的账号卡片视图（复刻 ZCode 的形态，但完全独立）。
 *
 * 为什么独立复刻而不复用 `CodebuddySuiteAccountsSharedView`：
 *   共享视图的接入配置有几十个必需字段，且它的齿轮写死了
 *   `quickSettingsType?: "codebuddy_cn" | "workbuddy" | "zcode" | "grok"` ——
 *   要让 Cindy 有自己的设置项就得扩那个联合类型、改共享组件，
 *   会给上游合并埋冲突。这里复制一份自己的，改什么都只影响 Cindy。
 *
 * 数据源：useCindyAccountStore（→ sidecars/cindy2api 的 HTTP 接口）。
 * 工具栏：搜索 / 全选 / `+` 添加账号 / 刷新 / 显隐 / 导出 JSON / 导入本机 / 设置。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  CheckSquare,
  ChevronLeft,
  ChevronRight,
  Download,
  Eye,
  EyeOff,
  FileJson,
  Plus,
  RefreshCw,
  Search,
  Settings,
  Square,
  Trash2,
  Upload,
} from 'lucide-react';

import '../../styles/pages/cindy.css';
import { AddCindyAccountDialog } from './AddCindyAccountDialog';
import {
  CINDY_SETTINGS_CHANGED_EVENT,
  CindySettingsDialog,
  readCindySettings,
} from './CindySettingsDialog';
import {
  getCindyDisplayName,
  getCindyPlanBadge,
  getCindyStatusText,
  getCindySubtitle,
  getCindyUsage,
  hasCindyQuotaData,
  isCindyAccountHealthy,
  summarizeCindyDetail,
  type CindyAccount,
} from '../../types/cindy';
import { useCindyAccountStore } from '../../stores/useCindyAccountStore';
import { fetchCredits, type CreditInfo } from '../../services/cindyService';

const VIEW_MODE_KEY = 'agtools.cindy.view_mode';

/** 时间戳 → yyyy/MM/dd HH:mm（与其它平台卡片一致） */
function formatTime(timestamp: number): string {
  if (!timestamp) return '—';
  const date = new Date(timestamp);
  const pad = (value: number) => String(value).padStart(2, '0');
  return `${date.getFullYear()}/${pad(date.getMonth() + 1)}/${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

export function CindyAccountsView() {
  const {
    accounts,
    loading,
    error,
    currentAccountId,
    fetchAccounts,
    setCurrentAccountId,
    refreshAllTokens,
    refreshToken,
    deleteAccounts,
    importLocal,
  } = useCindyAccountStore();

  const [query, setQuery] = useState('');
  const [selectedIds, setSelectedIds] = useState<string[]>([]);
  const [page, setPage] = useState(1);
  const [compact, setCompact] = useState(false);
  const [addOpen, setAddOpen] = useState(false);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState('');
  /** 设置（每页条数 / 自动刷新间隔 / 筛选记忆），由设置弹窗写入并广播变更 */
  const [settings, setSettings] = useState(readCindySettings);
  /** 账号额度（key = accountId）；本机账号没有 refreshToken，查不了，为 undefined */
  const [credits, setCredits] = useState<Record<string, CreditInfo>>({});
  const noticeTimer = useRef<number | null>(null);

  /** 顶部提示：3 秒后自动消失，避免一直占用版面 */
  const flash = useCallback((message: string) => {
    setNotice(message);
    if (noticeTimer.current !== null) window.clearTimeout(noticeTimer.current);
    noticeTimer.current = window.setTimeout(() => setNotice(''), 3000);
  }, []);

  /** 拉取账号列表，并顺带拉额度（额度接口要逐个刷新令牌，慢一些，别阻塞列表） */
  const load = useCallback(async () => {
    await fetchAccounts();
    try {
      const fetched = await fetchCredits();
      // 合并而非覆盖：额度查询要逐个刷新令牌走海外域名，网络一抖某个账号就会失败，
      // 直接覆盖会让原本显示的进度条闪没 —— 失败的账号保留上次成功的数据。
      setCredits((prev) => {
        const next = { ...prev };
        for (const [id, info] of Object.entries(fetched)) {
          if (!info.error) next[id] = info;
        }
        return next;
      });
    } catch {
      /* 整体失败时保留现有数据，不打断列表展示 */
    }
  }, [fetchAccounts]);

  useEffect(() => {
    void load();
  }, [load]);

  /** 设置变更后即时生效（设置弹窗保存时广播） */
  useEffect(() => {
    const handler = () => setSettings(readCindySettings());
    window.addEventListener(CINDY_SETTINGS_CHANGED_EVENT, handler);
    return () => window.removeEventListener(CINDY_SETTINGS_CHANGED_EVENT, handler);
  }, []);

  /** 按设置的间隔自动重新拉取账号状态（0 = 关闭） */
  useEffect(() => {
    const seconds = settings.autoRefreshSeconds;
    if (!seconds || seconds <= 0) return;
    const timer = window.setInterval(() => {
      void fetchAccounts();
    }, seconds * 1000);
    return () => window.clearInterval(timer);
  }, [settings.autoRefreshSeconds, fetchAccounts]);

  useEffect(() => {
    try {
      setCompact(window.localStorage.getItem(VIEW_MODE_KEY) === 'list');
    } catch {
      /* localStorage 不可用时用默认网格 */
    }
  }, []);

  const toggleCompact = useCallback(() => {
    setCompact((previous) => {
      const next = !previous;
      try {
        window.localStorage.setItem(VIEW_MODE_KEY, next ? 'list' : 'grid');
      } catch {
        /* 忽略持久化失败 */
      }
      return next;
    });
  }, []);

  /** 搜索过滤（名称 / 入口 / 脱敏 key） */
  const filtered = useMemo(() => {
    const keyword = query.trim().toLowerCase();
    if (!keyword) return accounts;
    return accounts.filter((account) =>
      [getCindyDisplayName(account), account.endpoint, account.apiKeyMasked]
        .join(' ')
        .toLowerCase()
        .includes(keyword),
    );
  }, [accounts, query]);

  const totalPages = Math.max(1, Math.ceil(filtered.length / settings.pageSize));
  const currentPage = Math.min(page, totalPages);
  const pageAccounts = filtered.slice(
    (currentPage - 1) * settings.pageSize,
    currentPage * settings.pageSize,
  );
  const allSelected = filtered.length > 0 && selectedIds.length === filtered.length;

  const toggleSelect = useCallback((id: string) => {
    setSelectedIds((previous) =>
      previous.includes(id) ? previous.filter((item) => item !== id) : [...previous, id],
    );
  }, []);

  const toggleSelectAll = useCallback(() => {
    setSelectedIds((previous) => (previous.length === filtered.length ? [] : filtered.map((a) => a.id)));
  }, [filtered]);

  /** 全量刷新（重新探测）+ 重新拉取列表 */
  const handleRefresh = useCallback(async () => {
    setBusy(true);
    try {
      await refreshAllTokens();
      await fetchAccounts();
      flash('已重新探测全部账号');
    } finally {
      setBusy(false);
    }
  }, [refreshAllTokens, fetchAccounts, flash]);

  /** 导出账号 JSON —— 只导出可安全外发的字段，不含任何凭据 */
  const handleExport = useCallback(() => {
    const payload = {
      platform: 'cindy',
      exportedAt: new Date().toISOString(),
      count: filtered.length,
      accounts: filtered.map((account) => ({
        id: account.id,
        label: getCindyDisplayName(account),
        endpoint: account.endpoint,
        keyMasked: account.apiKeyMasked,
        source: account.source,
        status: account.status,
        statusDetail: account.statusDetail,
        modelCount: account.modelCount,
        latencyMs: account.latencyMs,
        lastCheckedAt: account.lastCheckedAt,
      })),
    };
    const blob = new Blob([JSON.stringify(payload, null, 2)], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url;
    link.download = `cindy-accounts-${new Date().toISOString().slice(0, 10)}.json`;
    link.click();
    URL.revokeObjectURL(url);
    flash(`已导出 ${filtered.length} 个账号（不含凭据）`);
  }, [filtered, flash]);

  /** 从本机 Cindy 登录态导入 */
  const handleImportLocal = useCallback(async () => {
    setBusy(true);
    try {
      const result = await importLocal();
      flash(result.added > 0 ? `已导入 ${result.added} 个新账号` : '本机登录态已是最新');
    } catch (importError) {
      flash(`导入失败：${importError instanceof Error ? importError.message : String(importError)}`);
    } finally {
      setBusy(false);
    }
  }, [importLocal, flash]);

  const handleDeleteSelected = useCallback(async () => {
    if (selectedIds.length === 0) return;
    setBusy(true);
    try {
      // 两类账号都能移除：授权账号删我们的记录，本机账号删客户端凭据
      // （等价于该账号在客户端登出，需重新登录）—— 分流在 cindyService.removeAccounts
      await deleteAccounts(selectedIds);
      const oauthCount = selectedIds.filter((id) => id.startsWith('oauth-')).length;
      const localCount = selectedIds.length - oauthCount;
      setSelectedIds([]);
      flash(
        localCount > 0
          ? `已删除 ${selectedIds.length} 个账号（其中 ${localCount} 个是客户端登录态，需重新登录才能恢复）`
          : `已删除 ${oauthCount} 个授权账号`,
      );
    } catch (deleteError) {
      flash(`移除失败：${deleteError instanceof Error ? deleteError.message : String(deleteError)}`);
    } finally {
      setBusy(false);
    }
  }, [selectedIds, deleteAccounts, flash]);

  return (
    <section className="cindy-accounts-view">
      {/* 说明卡（对齐 ZCode 的可折叠说明） */}
      <div className="cindy-notice">
        <strong>Cindy 账号管理说明</strong>
        <p>
          账号由本机 Cindy 桌面端登录态自动发现，也可通过 OAuth 授权添加；
          <strong>凭据只保存在本机的反代网关服务里，不会上传到任何第三方</strong>。
        </p>
      </div>

      {/* 工具栏 */}
      <div className="cindy-toolbar">
        <div className="cindy-search">
          <Search size={15} />
          <input
            value={query}
            onChange={(event) => {
              setQuery(event.target.value);
              setPage(1);
            }}
            placeholder="搜索 Cindy 账号"
          />
        </div>

        <button className="cindy-btn" onClick={toggleSelectAll} title="全选/取消全选">
          {allSelected ? <CheckSquare size={15} /> : <Square size={15} />}
          全选
        </button>
        <span className="cindy-count">共 {filtered.length} 个</span>

        <div className="cindy-toolbar-right">
          <button className="cindy-btn primary" onClick={() => setAddOpen(true)} title="添加账号">
            <Plus size={16} />
          </button>
          <button className="cindy-btn" onClick={() => void handleRefresh()} disabled={busy} title="重新探测全部账号">
            <RefreshCw size={15} className={busy ? 'spin' : ''} />
          </button>
          <button className="cindy-btn" onClick={toggleCompact} title={compact ? '切换为网格' : '切换为列表'}>
            {compact ? <EyeOff size={15} /> : <Eye size={15} />}
          </button>
          <button className="cindy-btn" onClick={handleExport} title="导出账号 JSON（不含凭据）">
            <Download size={15} />
          </button>
          <button className="cindy-btn" onClick={() => void handleImportLocal()} disabled={busy} title="从本机登录态导入">
            <Upload size={15} />
          </button>
          <button className="cindy-btn" onClick={handleDeleteSelected} disabled={selectedIds.length === 0} title="移除所选授权账号">
            <Trash2 size={15} />
          </button>
          <button className="cindy-btn" onClick={() => setSettingsOpen(true)} title="设置">
            <Settings size={15} />
          </button>
        </div>
      </div>

      {notice && <div className="cindy-notice-line">{notice}</div>}
      {error && <div className="cindy-notice-line error">{error}</div>}

      {/* 卡片网格 */}
      {pageAccounts.length === 0 ? (
        <div className="cindy-empty">
          <FileJson size={28} />
          <p>{loading ? '正在读取账号…' : '还没有 Cindy 账号'}</p>
          <button className="cindy-btn primary" onClick={() => setAddOpen(true)}>
            <Plus size={15} /> 添加账号
          </button>
        </div>
      ) : (
        <div className={compact ? 'cindy-grid list' : 'cindy-grid'}>
          {pageAccounts.map((account: CindyAccount) => {
            const selected = selectedIds.includes(account.id);
            const isCurrent = currentAccountId === account.id;
            const credit = credits[account.id];
            // 额度进度条：region 决定货币 —— CN 人民币、国际美元
            const currency = credit?.region === 'cn' ? '¥' : '$';
            const total = Number(credit?.total ?? 0);
            const used = Number(credit?.used ?? 0);
            // 进度条表达「剩余」：余额越多条越满，随使用变短 —— 别反着用"已用"占比
            const pct =
              total > 0 ? Math.min(100, Math.round((Number(credit?.available) / total) * 100)) : 0;
            const hasCredit = Boolean(credit?.available) && total > 0;
            return (
              <article
                key={account.id}
                className={`cindy-card${selected ? ' selected' : ''}${isCurrent ? ' current' : ''}`}
              >
                <header>
                  <button className="cindy-check" onClick={() => toggleSelect(account.id)}>
                    {selected ? <CheckSquare size={16} /> : <Square size={16} />}
                  </button>
                  <span className="cindy-name" title={account.endpoint}>
                    {getCindyDisplayName(account)}
                  </span>
                  <span className={`cindy-badge${account.source === 'oauth' ? ' oauth' : ''}`}>
                    {getCindyPlanBadge(account)}
                  </span>
                  {/* 区域标签：CN 人民币计价 / 国际 美元计价 */}
                  {credit?.region === 'cn' && <span className="cindy-badge cn">CN</span>}
                  {credit?.region === 'global' && <span className="cindy-badge intl">国际</span>}
                </header>

                <div className="cindy-row">
                  <span>状态</span>
                  <b className={isCindyAccountHealthy(account) ? 'ok' : ''}>
                    {getCindyStatusText(account)}
                  </b>
                </div>

                {/* 额度：余额 + 进度条（仅显示查到额度的账号；查不到的不显示条，
                    完整错误（含排查建议）放进 title，卡片上只显示首句，避免撑破布局） */}
                {hasCredit ? (
                  <div className="cindy-quota" title={credit?.error ?? undefined}>
                    <div className="cindy-quota-grid">
                      <span>余额</span>
                      <b>
                        {currency}
                        {Number(credit?.available).toFixed(2)}
                      </b>
                    </div>
                    <div className="cindy-progress">
                      <div className="cindy-progress-fill" style={{ width: `${pct}%` }} />
                    </div>
                    <div className="cindy-quota-sub">
                      剩余 {currency}
                      {Number(credit?.available).toFixed(2)} / {currency}
                      {total.toFixed(2)}（{pct}%） · 已用 {currency}
                      {used.toFixed(2)}
                    </div>
                  </div>
                ) : (
                  <div className="cindy-quota" title={account.statusDetail || undefined}>
                    {hasCindyQuotaData(account) ? (
                      <div className="cindy-quota-grid">
                        <span>可用模型</span>
                        <b>{account.modelCount}</b>
                      </div>
                    ) : (
                      <div className="cindy-quota-empty">{summarizeCindyDetail(account.statusDetail)}</div>
                    )}
                  </div>
                )}

                <div className="cindy-sub">{getCindySubtitle(account)}</div>

                <footer>
                  <span className="cindy-time">{formatTime(account.lastCheckedAt)}</span>
                  <span className="cindy-usage" title={account.statusDetail || undefined}>
                    {getCindyUsage(account)}
                  </span>
                  <div className="cindy-actions">
                    <button onClick={() => setCurrentAccountId(account.id)} title="设为当前账号">
                      <CheckSquare size={14} />
                    </button>
                    <button onClick={() => void refreshToken(account.id)} title="重新探测该账号">
                      <RefreshCw size={14} />
                    </button>
                  </div>
                </footer>
              </article>
            );
          })}
        </div>
      )}

      {/* 分页 */}
      <footer className="cindy-pager">
        <span>
          显示 {pageAccounts.length === 0 ? 0 : (currentPage - 1) * settings.pageSize + 1} -{' '}
          {(currentPage - 1) * settings.pageSize + pageAccounts.length} 条，共 {filtered.length} 条
        </span>
        <div>
          <button className="cindy-btn" onClick={() => setPage(currentPage - 1)} disabled={currentPage <= 1}>
            <ChevronLeft size={15} /> 上一页
          </button>
          <span>
            第 {currentPage} / {totalPages} 页
          </span>
          <button
            className="cindy-btn"
            onClick={() => setPage(currentPage + 1)}
            disabled={currentPage >= totalPages}
          >
            下一页 <ChevronRight size={15} />
          </button>
        </div>
      </footer>

      {/* onAdded 用 load()（列表+额度）：只调 fetchAccounts 的话，新加的账号
          下一轮 load 前不会有余额/进度条 —— 表现为"添加后没有进度条" */}
      <AddCindyAccountDialog open={addOpen} onClose={() => setAddOpen(false)} onAdded={() => void load()} />
      <CindySettingsDialog
        open={settingsOpen}
        onClose={() => setSettingsOpen(false)}
        onSaved={() => flash('设置已保存')}
      />
    </section>
  );
}
