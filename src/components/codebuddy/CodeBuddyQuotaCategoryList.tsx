import { useMemo, useState } from 'react';
import { createPortal } from 'react-dom';
import { useTranslation } from 'react-i18next';
import { ArrowRight, Clock, Sparkles, X } from 'lucide-react';
import { useEscClose } from '../../hooks/useEscClose';
import type { QuotaCategoryGroup, CodebuddyOfficialQuotaResource } from '../../types/codebuddy';

interface CodeBuddyQuotaCategoryListProps {
  /** 配额分组数据（含各分组下的积分包明细） */
  groups: QuotaCategoryGroup[];
  /** 数字格式化（千分位 + 最多两位小数） */
  formatNumber: (value: number) => string;
  /** 完整日期时间格式化（用于"更新时间"的悬停提示） */
  formatDateTime: (timeMs: number | null) => string;
  /** 配额数据最后更新时间（毫秒时间戳，可为空） */
  updatedAtMs?: number | null;
  /** 账号显示名（弹窗副标题用，如脱敏邮箱/昵称） */
  accountLabel?: string;
}

/** 带分组兜底名称的积分包（包名为空时用分组名兜底展示） */
type CreditPack = CodebuddyOfficialQuotaResource & { fallbackName: string };

/** 卡片内固定展示的积分包行数（与 wb2api 一致） */
const VISIBLE_PACK_COUNT = 2;
/** 距到期不足 7 天视为"即将到期"（展示为橙色） */
const EXPIRING_SOON_MS = 7 * 24 * 60 * 60 * 1000;

/**
 * 从所有分组中收集"可用"积分包：
 * 已用完（剩余 <= 0 且非无限额度）的包不展示，与 wb2api 保持一致；
 * 结果按到期时间升序排列，无到期时间的排在最后。
 */
function collectActivePacks(groups: QuotaCategoryGroup[]): CreditPack[] {
  const packs: CreditPack[] = [];
  for (const group of groups) {
    for (const item of group.items) {
      if (item.unlimited || item.remain > 0) {
        packs.push({ ...item, fallbackName: group.label });
      }
    }
  }
  return packs.sort(
    (a, b) => (a.expireAt ?? Number.POSITIVE_INFINITY) - (b.expireAt ?? Number.POSITIVE_INFINITY),
  );
}

/** 计算包的到期状态：expired=已过期（红）、soon=7 天内到期（橙）、normal=正常 */
function resolveExpireState(pack: CreditPack, now: number): 'expired' | 'soon' | 'normal' {
  if (pack.expireAt == null) return 'normal';
  if (pack.expireAt < now) return 'expired';
  if (pack.expireAt - now <= EXPIRING_SOON_MS) return 'soon';
  return 'normal';
}

/**
 * 配额积分包展示（wb2api 风格）
 * 顶部汇总：可用积分 + 积分包数量 + 最后更新时间；
 * "近期到期"固定展示前 2 个包；超过 2 个时显示"查看全部积分包"入口，
 * 点击弹出弹窗查看全部积分包详情（剩余/总量/已用/到期日期）。
 */
export function CodeBuddyQuotaCategoryList({
  groups,
  formatNumber,
  formatDateTime,
  updatedAtMs = null,
  accountLabel = '',
}: CodeBuddyQuotaCategoryListProps) {
  const { t, i18n } = useTranslation();
  // "全部积分包"弹窗开关
  const [allPacksOpen, setAllPacksOpen] = useState(false);
  useEscClose(allPacksOpen, () => setAllPacksOpen(false));

  // 可用积分包列表（已过滤用完的包，按到期时间升序）
  const activePacks = useMemo(() => collectActivePacks(groups), [groups]);

  // 顶部汇总：可用积分与包数量（无限额度包不计入数值，仅计数并标记）
  const summary = useMemo(() => {
    let available = 0;
    let hasUnlimited = false;
    for (const pack of activePacks) {
      if (pack.unlimited) {
        hasUnlimited = true;
        continue;
      }
      available += pack.remain;
    }
    return { available, hasUnlimited, count: activePacks.length };
  }, [activePacks]);

  // 卡片内固定只展示前 2 个包
  const visiblePacks = useMemo(() => activePacks.slice(0, VISIBLE_PACK_COUNT), [activePacks]);

  // 到期日期只显示 月/日（如 10/10），弹窗内用完整日期
  const expireDateFormatter = useMemo(
    () => new Intl.DateTimeFormat(i18n.language || undefined, { month: '2-digit', day: '2-digit' }),
    [i18n.language],
  );
  const expireFullDateFormatter = useMemo(
    () => new Intl.DateTimeFormat(i18n.language || undefined, { year: 'numeric', month: '2-digit', day: '2-digit' }),
    [i18n.language],
  );
  // 更新时间只显示 时:分（如 17:23）
  const updateTimeFormatter = useMemo(
    () => new Intl.DateTimeFormat(i18n.language || undefined, { hour: '2-digit', minute: '2-digit', hour12: false }),
    [i18n.language],
  );

  if (activePacks.length === 0) {
    return (
      <div className="quota-category-empty">
        {t('common.shared.quota.noActivePackages', '暂无可用积分包')}
      </div>
    );
  }

  return (
    <div className="quota-credit-panel">
      {/* 顶部汇总：可用积分 + 积分包数量 + 最后更新时间 */}
      <div className="quota-credit-summary">
        <div className="quota-credit-summary-main">
          <Sparkles size={16} className="quota-credit-summary-icon" />
          <span className="quota-credit-available">
            {summary.hasUnlimited ? '∞' : formatNumber(summary.available)}
          </span>
          <span className="quota-credit-summary-meta">
            {t('common.shared.quota.packagesCount', '{{count}} 个积分包', { count: summary.count })}
          </span>
        </div>
        {updatedAtMs != null && (
          <span
            className="quota-credit-updated"
            title={t('common.shared.quota.updatedAtFull', '最后更新：{{time}}', {
              time: formatDateTime(updatedAtMs),
            })}
          >
            <Clock size={12} />
            {t('common.shared.quota.updatedAtShort', '{{time}} 更新', {
              time: updateTimeFormatter.format(updatedAtMs),
            })}
          </span>
        )}
      </div>

      {/* 近期到期：固定展示前 2 个包，剩余的进弹窗查看 */}
      <div className="quota-credit-expiring">{t('common.shared.quota.expiringSoon', '近期到期')}</div>
      <div className="quota-credit-rows">
        {visiblePacks.map((pack, idx) => (
          <CreditPackRow
            key={`${pack.packageCode || 'pkg'}-${pack.expireAt ?? 'na'}-${idx}`}
            pack={pack}
            formatNumber={formatNumber}
            expireDateFormatter={expireDateFormatter}
            expireFullDateFormatter={expireFullDateFormatter}
          />
        ))}
      </div>

      {/* 超过 2 个包时显示"查看全部积分包"入口 */}
      {activePacks.length > VISIBLE_PACK_COUNT && (
        <button type="button" className="quota-credit-more" onClick={() => setAllPacksOpen(true)}>
          {t('common.shared.quota.viewAllPacks', '查看全部积分包')}
          <ArrowRight size={14} />
        </button>
      )}

      {/* 全部积分包弹窗：用 Portal 挂到 body，避免被卡片祖先的 transform 困住导致不居中/被裁剪 */}
      {allPacksOpen &&
        createPortal(
          <div
            className="modal-overlay"
            role="presentation"
            onMouseDown={() => setAllPacksOpen(false)}
            onClick={() => setAllPacksOpen(false)}
          >
            <div
              className="modal quota-packs-modal"
              role="dialog"
              aria-modal="true"
              aria-label={t('common.shared.quota.allPacksTitle', '全部积分包')}
              onClick={(e) => e.stopPropagation()}
              onMouseDown={(e) => e.stopPropagation()}
            >
              <div className="modal-header">
                <h2 className="quota-packs-modal-title">
                  {t('common.shared.quota.allPacksTitle', '全部积分包')}
                </h2>
                <button
                  type="button"
                  className="modal-close"
                  onClick={() => setAllPacksOpen(false)}
                  aria-label={t('common.close', '关闭')}
                >
                  <X size={18} />
                </button>
              </div>
              <div className="modal-body quota-packs-modal-body">
                <p className="quota-packs-modal-subtitle">
                  {t('common.shared.quota.allPacksSubtitle', '{{label}} · 共 {{count}} 个积分包', {
                    label: accountLabel,
                    count: activePacks.length,
                  })}
                </p>
                <div className="quota-packs-list">
                  {activePacks.map((pack, idx) => (
                    <CreditPackDetailItem
                      key={`${pack.packageCode || 'pkg'}-${pack.expireAt ?? 'na'}-${idx}`}
                      pack={pack}
                      formatNumber={formatNumber}
                      expireFullDateFormatter={expireFullDateFormatter}
                    />
                  ))}
                </div>
              </div>
            </div>
          </div>,
          document.body,
        )}
    </div>
  );
}

interface CreditPackRowProps {
  /** 单个积分包数据（含分组兜底名称） */
  pack: CreditPack;
  /** 数字格式化 */
  formatNumber: (value: number) => string;
  /** 到期日期格式化（月/日） */
  expireDateFormatter: Intl.DateTimeFormat;
  /** 到期日期格式化（年/月/日，悬停提示用） */
  expireFullDateFormatter: Intl.DateTimeFormat;
}

/** 卡片内的单个积分包行：剩余积分 + 包名 + 到期时间 + 剩余进度条 */
function CreditPackRow({
  pack,
  formatNumber,
  expireDateFormatter,
  expireFullDateFormatter,
}: CreditPackRowProps) {
  const { t } = useTranslation();
  // 进度条宽度 = 剩余占比（无限额度显示满格）
  const remainPercent = pack.unlimited
    ? 100
    : pack.total > 0
      ? Math.max(0, Math.min(100, (pack.remain / pack.total) * 100))
      : 0;
  // 到期状态：已过期（红）/ 7 天内到期（橙）/ 正常
  const expireState = resolveExpireState(pack, Date.now());
  const packName = pack.packageName || pack.fallbackName;

  return (
    <div
      className="quota-credit-row"
      title={`${packName} · ${t('common.shared.quota.creditsValue', '{{value}} 积分', {
        value: formatNumber(pack.remain),
      })} · ${
        pack.expireAt != null
          ? expireFullDateFormatter.format(pack.expireAt)
          : t('common.shared.quota.longTerm', '长期有效')
      }`}
    >
      <div className="quota-credit-row-header">
        <span className="quota-credit-row-value">
          {pack.unlimited
            ? t('common.shared.quota.unlimited', '无限额度')
            : t('common.shared.quota.creditsValue', '{{value}} 积分', { value: formatNumber(pack.remain) })}
        </span>
        <span className="quota-credit-row-name">{packName}</span>
        <span className={`quota-credit-row-expire ${expireState}`}>
          {pack.expireAt != null
            ? t('common.shared.quota.expiresOn', '{{date}} 到期', {
                date: expireDateFormatter.format(pack.expireAt),
              })
            : t('common.shared.quota.longTerm', '长期有效')}
        </span>
      </div>
      <div className="quota-credit-row-bar">
        <div
          className={`quota-credit-row-bar-fill ${expireState}`}
          style={{ width: `${remainPercent}%` }}
        />
      </div>
    </div>
  );
}

interface CreditPackDetailItemProps {
  /** 弹窗内的单个积分包数据 */
  pack: CreditPack;
  /** 数字格式化 */
  formatNumber: (value: number) => string;
  /** 到期日期格式化（年/月/日） */
  expireFullDateFormatter: Intl.DateTimeFormat;
}

/** 弹窗内的单个积分包详情：包名 + 剩余/总量 + 已用 + 完整到期日期 + 进度条 */
function CreditPackDetailItem({
  pack,
  formatNumber,
  expireFullDateFormatter,
}: CreditPackDetailItemProps) {
  const { t } = useTranslation();
  // 进度条宽度 = 剩余占比（无限额度显示满格）
  const remainPercent = pack.unlimited
    ? 100
    : pack.total > 0
      ? Math.max(0, Math.min(100, (pack.remain / pack.total) * 100))
      : 0;
  // 到期状态：已过期（红）/ 7 天内到期（橙）/ 正常
  const expireState = resolveExpireState(pack, Date.now());

  return (
    <div className="quota-pack-item">
      <div className="quota-pack-item-header">
        <div className="quota-pack-item-info">
          <span className="quota-pack-item-name" title={pack.packageName || pack.fallbackName}>
            {pack.packageName || pack.fallbackName || t('common.shared.quota.defaultPackName', '积分包')}
          </span>
          <span className={`quota-pack-item-date ${expireState}`}>
            {pack.expireAt == null
              ? t('common.shared.quota.longTerm', '长期有效')
              : expireState === 'expired'
                ? t('common.shared.quota.alreadyExpired', '已到期')
                : expireState === 'soon'
                  ? t('common.shared.quota.expiringIn7Days', '7 天内到期')
                  : t('common.shared.quota.expiresFull', '到期 {{date}}', {
                      date: expireFullDateFormatter.format(pack.expireAt),
                    })}
          </span>
        </div>
        <div className="quota-pack-item-amounts">
          <span className="quota-pack-item-amount">
            {pack.unlimited
              ? t('common.shared.quota.unlimited', '无限额度')
              : `${formatNumber(pack.remain)} / ${formatNumber(pack.total)}`}
          </span>
          <span className="quota-pack-item-used">
            {t('common.shared.quota.usedValue', '已用 {{value}}', { value: formatNumber(pack.used) })}
          </span>
        </div>
      </div>
      <div className="quota-credit-row-bar">
        <div
          className={`quota-credit-row-bar-fill ${expireState}`}
          style={{ width: `${remainPercent}%` }}
        />
      </div>
    </div>
  );
}
