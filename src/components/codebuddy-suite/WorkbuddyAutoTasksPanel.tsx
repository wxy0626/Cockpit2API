import { useCallback, useEffect, useMemo, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { listen } from '@tauri-apps/api/event';
import {
  AlertCircle,
  CheckCircle2,
  ChevronDown,
  ChevronUp,
  ListChecks,
  Loader2,
  Play,
  RefreshCw,
  Trash2,
} from 'lucide-react';
import {
  WORKBUDDY_AUTO_TASKS_CONFIG_CHANGED_EVENT,
  WORKBUDDY_AUTO_TASKS_LOGS_CHANGED_EVENT,
  clearWorkbuddyAutoTasksLogs,
  getWorkbuddyAutoTasksConfig,
  getWorkbuddyAutoTasksConfigAsync,
  getWorkbuddyAutoTasksLogsAsync,
  isValidTimeString,
  runWorkbuddyAutoTasksCycleIfNeeded,
  saveWorkbuddyAutoTasksConfigAsync,
  type WorkbuddyAutoTasksConfig,
  type WorkbuddyAutoTasksLogRecord,
} from '../../services/workbuddyAutoTasksService';
import '../../styles/pages/workbuddy-auto-tasks.css';

/**
 * WorkBuddy「自动任务」面板。
 *
 * 职责：展示与编辑自动任务配置、查看运行记录、手动触发一轮。
 * 调度与执行全部在 Rust 后端，本组件不含任何任务逻辑。
 *
 * 合规说明（刻意保留在界面上，避免用户误解）：
 * 本功能只调用「领取/动作类」接口，由服务端裁定结果；
 * 不实现任何需要客户端自证行为的上报，因此不会伪造行为数据。
 */
export function WorkbuddyAutoTasksPanel() {
  const { t } = useTranslation();

  const [config, setConfig] = useState<WorkbuddyAutoTasksConfig>(getWorkbuddyAutoTasksConfig);
  const [logs, setLogs] = useState<WorkbuddyAutoTasksLogRecord[]>([]);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [running, setRunning] = useState(false);
  const [notice, setNotice] = useState<{ tone: 'ok' | 'err'; text: string } | null>(null);
  const [expandedIds, setExpandedIds] = useState<Record<string, boolean>>({});

  /** 拉取配置与日志 */
  const load = useCallback(async () => {
    setLoading(true);
    try {
      const [nextConfig, nextLogs] = await Promise.all([
        getWorkbuddyAutoTasksConfigAsync(),
        getWorkbuddyAutoTasksLogsAsync(),
      ]);
      setConfig(nextConfig);
      setLogs(nextLogs);
    } catch (err) {
      setNotice({ tone: 'err', text: `${err}` });
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // 订阅后端事件：调度器跑完一轮后自动刷新界面
  useEffect(() => {
    const disposers: Array<() => void> = [];
    let disposed = false;
    const bind = async () => {
      const offConfig = await listen(WORKBUDDY_AUTO_TASKS_CONFIG_CHANGED_EVENT, () => {
        void getWorkbuddyAutoTasksConfigAsync().then(setConfig);
      });
      const offLogs = await listen(WORKBUDDY_AUTO_TASKS_LOGS_CHANGED_EVENT, () => {
        void getWorkbuddyAutoTasksLogsAsync().then(setLogs);
      });
      if (disposed) {
        offConfig();
        offLogs();
        return;
      }
      disposers.push(offConfig, offLogs);
    };
    void bind();
    return () => {
      disposed = true;
      disposers.forEach((off) => off());
    };
  }, []);

  /** 校验并保存配置 */
  const save = useCallback(
    async (next: WorkbuddyAutoTasksConfig) => {
      if (!isValidTimeString(next.startTime) || !isValidTimeString(next.endTime)) {
        setNotice({ tone: 'err', text: t('workbuddy.autoTasks.timeInvalid', '执行窗口时间格式无效') });
        return;
      }
      setSaving(true);
      try {
        await saveWorkbuddyAutoTasksConfigAsync(next);
        setConfig(next);
        setNotice({ tone: 'ok', text: t('workbuddy.autoTasks.saved', '设置已保存') });
      } catch (err) {
        setNotice({ tone: 'err', text: `${err}` });
      } finally {
        setSaving(false);
      }
    },
    [t],
  );

  /** 编辑配置：本地即时生效，等待用户点保存落盘 */
  const patch = useCallback((partial: Partial<WorkbuddyAutoTasksConfig>) => {
    setConfig((prev) => ({ ...prev, ...partial }));
  }, []);

  /** 手动立即执行一轮（忽略执行窗口限制） */
  const runNow = useCallback(async () => {
    setRunning(true);
    setNotice(null);
    try {
      const result = await runWorkbuddyAutoTasksCycleIfNeeded(true);
      const text =
        result === 'disabled'
          ? t('workbuddy.autoTasks.runDisabled', '总开关未开启，已跳过')
          : result === 'already_running'
            ? t('workbuddy.autoTasks.runBusy', '已有一轮正在执行中')
            : t('workbuddy.autoTasks.runDone', '执行完成，请查看运行记录');
      setNotice({ tone: result === 'disabled' ? 'err' : 'ok', text });
      await load();
    } catch (err) {
      setNotice({ tone: 'err', text: `${err}` });
    } finally {
      setRunning(false);
    }
  }, [load, t]);

  /** 清空运行记录 */
  const clearLogs = useCallback(async () => {
    try {
      await clearWorkbuddyAutoTasksLogs();
      setLogs([]);
    } catch (err) {
      setNotice({ tone: 'err', text: `${err}` });
    }
  }, []);

  const lastRunText = useMemo(() => {
    if (!config.lastRunDate) {
      return t('workbuddy.autoTasks.neverRun', '尚未执行');
    }
    return config.lastRunDate;
  }, [config.lastRunDate, t]);

  const renderToggle = (
    label: string,
    hint: string,
    value: boolean,
    onChange: (next: boolean) => void,
  ) => (
    <label className="wb-tasks-toggle">
      <input type="checkbox" checked={value} onChange={(e) => onChange(e.target.checked)} />
      <span className="wb-tasks-toggle-text">
        <b>{label}</b>
        <em>{hint}</em>
      </span>
    </label>
  );

  return (
    <section className="workbuddy-auto-tasks-panel">
      <header className="wb-tasks-header">
        <h2>
          <ListChecks size={18} />
          {t('workbuddy.autoTasks.title', '自动任务')}
          <span className={`wb-tasks-state${config.enabled ? ' on' : ''}`}>
            {config.enabled
              ? t('workbuddy.autoTasks.enabled', '已开启')
              : t('workbuddy.autoTasks.disabled', '已关闭')}
          </span>
        </h2>
        <div className="wb-tasks-header-actions">
          <button onClick={() => void load()} disabled={loading} title={t('common.refresh', '刷新')}>
            <RefreshCw size={15} className={loading ? 'spin' : ''} />
          </button>
          <button onClick={() => void runNow()} disabled={running}>
            {running ? <Loader2 size={15} className="spin" /> : <Play size={15} />}
            {t('workbuddy.autoTasks.runNow', '立即执行一轮')}
          </button>
        </div>
      </header>

      {notice && (
        <div className={`wb-tasks-notice ${notice.tone}`}>
          {notice.tone === 'ok' ? <CheckCircle2 size={15} /> : <AlertCircle size={15} />}
          <span>{notice.text}</span>
        </div>
      )}

      <div className="wb-tasks-card">
        <div className="wb-tasks-row">
          <strong>{t('workbuddy.autoTasks.masterSwitch', '自动任务总开关')}</strong>
          {renderToggle(
            config.enabled
              ? t('workbuddy.autoTasks.on', '开启')
              : t('workbuddy.autoTasks.off', '关闭'),
            t(
              'workbuddy.autoTasks.masterHint',
              '开启后每天在执行窗口内自动跑一轮（默认 07:00-12:00）',
            ),
            config.enabled,
            (next) => patch({ enabled: next }),
          )}
        </div>

        <div className="wb-tasks-row">
          <strong>{t('workbuddy.autoTasks.window', '每日执行窗口')}</strong>
          <div className="wb-tasks-window">
            <input
              type="time"
              value={config.startTime}
              onChange={(e) => patch({ startTime: e.target.value })}
            />
            <span>—</span>
            <input
              type="time"
              value={config.endTime}
              onChange={(e) => patch({ endTime: e.target.value })}
            />
            <em>{t('workbuddy.autoTasks.windowHint', '应用需处于运行状态')}</em>
          </div>
        </div>

        <div className="wb-tasks-row">
          <strong>{t('workbuddy.autoTasks.scope', '执行范围')}</strong>
          <div className="wb-tasks-scope">
            {renderToggle(
              t('workbuddy.autoTasks.runTasks', '完成任务'),
              t('workbuddy.autoTasks.runTasksHint', '接受未接任务、复核进度、领取已达成的奖励'),
              config.runTasks,
              (next) => patch({ runTasks: next }),
            )}
            {renderToggle(
              t('workbuddy.autoTasks.runPlay', '互动玩法'),
              t('workbuddy.autoTasks.runPlayHint', '抽奖、连签兑换、补签卡、新手礼包、活动补偿（均为免费收益）'),
              config.runPlay,
              (next) => patch({ runPlay: next }),
            )}
            {renderToggle(
              t('workbuddy.autoTasks.runBlindbox', '开启盲盒'),
              t('workbuddy.autoTasks.runBlindboxHint', '消耗能量，默认关闭'),
              config.runBlindbox,
              (next) => patch({ runBlindbox: next }),
            )}
            {renderToggle(
              t('workbuddy.autoTasks.autoAdopt', '自动同意领养协议'),
              t(
                'workbuddy.autoTasks.autoAdoptHint',
                '任务链的前置节点，不同意则后续任务全部无法解锁（等同于你在客户端点同意）',
              ),
              config.autoAdoptAgreement,
              (next) => patch({ autoAdoptAgreement: next }),
            )}
            {renderToggle(
              t('workbuddy.autoTasks.runTheme', '体验和平精英主题'),
              t(
                'workbuddy.autoTasks.runThemeHint',
                '设置和平精英主题完成任务 Hp_Appearance。会真的切换你的客户端主题且无法还原；且该项必须同时开启「行为上报通道」才能真正完成',
              ),
              config.runThemeTask,
              (next) => patch({ runThemeTask: next }),
            )}
            {renderToggle(
              t('workbuddy.autoTasks.enableReporting', '行为上报通道'),
              t(
                'workbuddy.autoTasks.enableReportingHint',
                '为「设计创意/模板/专家/聊天」等任务上报行为事件以使其完成。代价：这些事件由本工具代替客户端声明，服务端无法核实。不开则这些任务必须手动完成',
              ),
              config.enableReporting,
              (next) => patch({ enableReporting: next }),
            )}
          </div>
        </div>

        <div className="wb-tasks-actions">
          <button className="primary" onClick={() => void save(config)} disabled={saving}>
            {saving ? <Loader2 size={15} className="spin" /> : null}
            {t('common.save', '保存设置')}
          </button>
          <span className="wb-tasks-meta">
            {t('workbuddy.autoTasks.lastRun', '最近执行')}: {lastRunText}
          </span>
        </div>
      </div>

      <div className="wb-tasks-note">
        {t(
          'workbuddy.autoTasks.complianceNote',
          '本功能只调用官方的领取/动作类接口，任务是否完成由服务端判定，不上报任何行为数据。聊天类任务（如「和 AI 聊天」）经实测无法自动完成，需手动处理。',
        )}
      </div>

      <div className="wb-tasks-card">
        <div className="wb-tasks-logs-header">
          <strong>{t('workbuddy.autoTasks.logs', '运行记录')}</strong>
          <button onClick={() => void clearLogs()} disabled={logs.length === 0}>
            <Trash2 size={14} />
            {t('common.shared.clear', '清空')}
          </button>
        </div>

        {logs.length === 0 ? (
          <p className="wb-tasks-empty">
            {t('workbuddy.autoTasks.noLogs', '暂无运行记录')}
          </p>
        ) : (
          <ul className="wb-tasks-logs">
            {logs.map((log) => {
              const expanded = Boolean(expandedIds[log.id]);
              return (
                <li key={log.id} className={`wb-tasks-log ${log.status}`}>
                  <button
                    className="wb-tasks-log-head"
                    onClick={() =>
                      setExpandedIds((prev) => ({ ...prev, [log.id]: !prev[log.id] }))
                    }
                  >
                    <span className="wb-tasks-log-time">{log.timestamp}</span>
                    <span className="wb-tasks-log-summary">
                      {t('workbuddy.autoTasks.logSummary', '账号 {{total}} · 成功 {{ok}} · 失败 {{fail}}', {
                        total: log.totalAccounts,
                        ok: log.successCount,
                        fail: log.failedCount,
                      })}
                      <em>{Math.max(1, Math.round(log.durationMs / 1000))}s</em>
                    </span>
                    {expanded ? <ChevronUp size={14} /> : <ChevronDown size={14} />}
                  </button>
                  {expanded && (
                    <ul className="wb-tasks-log-details">
                      {log.details.map((detail) => (
                        <li key={detail.accountId}>
                          <div className="wb-tasks-detail-head">
                            <b>{detail.email || detail.accountId}</b>
                            <span className="wb-tasks-detail-progress">
                              {detail.progressSummary}
                            </span>
                          </div>
                          <div className="wb-tasks-detail-message">{detail.message}</div>
                        </li>
                      ))}
                    </ul>
                  )}
                </li>
              );
            })}
          </ul>
        )}
      </div>
    </section>
  );
}

export default WorkbuddyAutoTasksPanel;
