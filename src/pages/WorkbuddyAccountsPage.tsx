import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { PlatformOverviewTabsHeader, PlatformOverviewTab } from '../components/platform/PlatformOverviewTabsHeader';
import { WorkbuddyInstancesContent } from './WorkbuddyInstancesPage';
import { useWorkbuddyAccountStore } from '../stores/useWorkbuddyAccountStore';
import * as workbuddyService from '../services/workbuddyService';
import { syncWorkbuddyToCodebuddyCn } from '../services/codebuddyCnService';
import {
  WorkbuddyAccount,
  getWorkbuddyAccountDisplayEmail,
  getWorkbuddyPlanBadge,
  getWorkbuddyUsage,
  getWorkbuddyQuotaCategoryGroups,
} from '../types/workbuddy';
import { useProviderAccountsPage } from '../hooks/useProviderAccountsPage';
import { WorkbuddyCheckinModal } from '../components/codebuddy-suite/CodebuddySuiteCheckinModal';
import { CodebuddySessionManager } from '../components/codebuddy/CodebuddySessionManager';
import { CodebuddySuiteAccountsSharedView, type CodebuddySuiteAccountsPlatformConfig } from '../components/codebuddy-suite/CodebuddySuiteAccountsSharedView';
// WorkBuddy「自动任务」面板（独立模块，与上游代码解耦）
import { WorkbuddyAutoTasksPanel } from '../components/codebuddy-suite/WorkbuddyAutoTasksPanel';
import { compareCurrentAccountFirst } from '../utils/currentAccountSort';
import { listen } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';
import {
  getWorkbuddyAutoCheckinConfig,
  getWorkbuddyAutoCheckinConfigAsync,
  getWorkbuddyAutoCheckinLogsAsync,
  getLocalTodayStr,
  formatMinuteOfDay,
  formatTodayTimestamp,
  WORKBUDDY_AUTO_CHECKIN_CONFIG_CHANGED_EVENT,
  WORKBUDDY_AUTO_CHECKIN_LOGS_CHANGED_EVENT,
  WorkbuddyAutoCheckinConfig,
} from '../services/workbuddyAutoCheckinService';

import { Check, ChevronDown, CircleCheck, Copy, Eye, EyeOff, Gauge, PlaneTakeoff, Play, RefreshCw } from 'lucide-react';

const ADMIN_BASE = 'http://127.0.0.1:7864';
const DEFAULT_BASE = 'http://127.0.0.1:7863/v1';
interface Config { config?: Record<string, unknown>; baseUrl?: string; lan_base_url?: string | null }
interface Status { total?: number; healthy?: number; accounts?: unknown[] }

/** 网关账号池单账号状态（/api/status 里 accounts[] 的字段子集，仅供并发上限设置用） */
interface WbPoolAccount {
  uid?: string;
  nickname?: string;
  // 在途请求数 / 当前生效的并发上限（0 或缺省 = 不限）
  in_flight?: number;
  max_in_flight?: number;
  // 是否已打满上限（打满后 Pick 会跳过该账号）
  in_flight_full?: boolean;
  cooling?: boolean;
  disabled?: boolean;
}

/** 单账号并发上限行：展示在途/上限，输入新值保存（0 = 跟随全局，清除覆盖） */
function WbPoolAccountLimitRow({ acct, onSaved }: { acct: WbPoolAccount; onSaved: (msg: string) => void }) {
  const [limit, setLimit] = useState<string>(acct.max_in_flight ? String(acct.max_in_flight) : '0');
  const [saving, setSaving] = useState(false);
  const save = async () => {
    setSaving(true);
    try {
      const r = await fetch(`${ADMIN_BASE}/api/account/max-in-flight`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ uid: acct.uid, limit: Number(limit) || 0 }),
      }).then((x) => x.json() as Promise<{ ok?: boolean; message?: string }>);
      onSaved(r.ok ? `${acct.nickname || acct.uid}：${r.message}` : `失败：${r.message}`);
    } catch (e) {
      onSaved(`失败：${e instanceof Error ? e.message : String(e)}`);
    } finally {
      setSaving(false);
    }
  };
  const stateText = acct.disabled ? '已禁用' : acct.cooling ? '冷却中' : acct.in_flight_full ? '已满载' : '正常';
  return (
    <div className="wb-row">
      <strong title={acct.uid}>{acct.nickname || acct.uid || '?'}</strong>
      <code title="在途 / 生效上限">{acct.in_flight ?? 0}/{acct.max_in_flight || '∞'}</code>
      <span title="账号状态">{stateText}</span>
      <input
        type="number" min={0} max={99} value={limit} disabled={saving}
        title="并发上限，0 = 跟随全局（默认 3）"
        style={{ width: 56 }}
        onChange={(e) => setLimit(e.target.value)}
      />
      <button onClick={() => void save()} disabled={saving}>保存</button>
    </div>
  );
}

/** 网关账号池并发上限设置区：读取 /api/status 的账号列表，逐账号设置并发覆盖 */
function WbPoolAccountLimits() {
  const [accounts, setAccounts] = useState<WbPoolAccount[]>([]);
  const [msg, setMsg] = useState('');
  const load = async () => {
    try {
      const s = await fetch(`${ADMIN_BASE}/api/status`).then((r) => r.json() as Promise<Status>);
      setAccounts((s.accounts ?? []) as WbPoolAccount[]);
    } catch (e) {
      setMsg(`无法读取账号状态：${e instanceof Error ? e.message : String(e)}`);
    }
  };
  useEffect(() => { void load(); }, []);
  return (
    <details className="wb-pool-limits">
      <summary>账号并发上限（打满自动跳过，0 = 跟随全局默认 3）</summary>
      {accounts.map((a) => (
        <WbPoolAccountLimitRow key={a.uid} acct={a} onSaved={(m) => { setMsg(m); void load(); }} />
      ))}
      {msg && <div className="wb-meta"><span>{msg}</span></div>}
    </details>
  );
}

/** 网关账号池并发状态 hook：拉一次 /api/status，提供 uid→账号状态映射与刷新 */
function useWbPoolLimits() {
  const [map, setMap] = useState<Record<string, WbPoolAccount>>({});
  const reload = useCallback(async () => {
    try {
      const s = await fetch(`${ADMIN_BASE}/api/status`).then((r) => r.json() as Promise<Status>);
      const next: Record<string, WbPoolAccount> = {};
      for (const a of (s.accounts ?? []) as WbPoolAccount[]) {
        if (a.uid) next[a.uid] = a;
      }
      setMap(next);
    } catch {
      // 网关未运行：映射留空，卡片输入框仍可编辑
    }
  }, []);
  useEffect(() => { void reload(); }, [reload]);
  return { map, reload };
}

/** 卡片内联并发上限输入框：value 为当前生效上限（0 = 跟随全局默认 3），Enter 或失焦保存 */
function WbCardLimitInput({ uid, value, inFlight, onSaved }: { uid: string; value: number; inFlight: number; onSaved: () => void }) {
  const [text, setText] = useState(value ? String(value) : '0');
  const [saving, setSaving] = useState(false);
  // 网关状态刷新后同步回显值
  useEffect(() => { setText(value ? String(value) : '0'); }, [value]);
  const save = async () => {
    const n = Math.max(0, Math.min(99, Number(text) || 0));
    if (n === value) return; // 无变化不请求
    setSaving(true);
    try {
      await fetch(`${ADMIN_BASE}/api/account/max-in-flight`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ uid, limit: n }),
      });
      onSaved();
    } catch {
      // 网关未运行时保存失败，静默保留输入值
    } finally {
      setSaving(false);
    }
  };
  return (
    <span
      style={{ display: 'inline-flex', alignItems: 'center', gap: 3, marginLeft: 'auto' }}
      title={`在途 ${inFlight} · 并发上限（0 = 跟随全局默认 3）`}
    >
      <Gauge size={13} style={{ opacity: 0.6 }} />
      <input
        type="number" min={0} max={99} value={text} disabled={saving}
        style={{ width: 48, padding: '2px 4px', fontSize: 12, borderRadius: 6, border: '1px solid rgba(128,128,128,0.35)' }}
        onChange={(e) => setText(e.target.value)}
        onBlur={() => void save()}
        onKeyDown={(e) => { if (e.key === 'Enter') (e.target as HTMLInputElement).blur(); }}
      />
    </span>
  );
}

/** 本地账号库可用判定：已配置 access_token 且未过期（expires_at 为毫秒时间戳，缺失时视为可用，可由刷新任务续期）。 */
const isWorkbuddyAccountReady = (account: WorkbuddyAccount): boolean =>
  Boolean(account.access_token) && (account.expires_at == null || account.expires_at > Date.now());

/** 判断账号今日是否已签到（last_checkin_time 为秒级时间戳，按本地日期比较） */
function isWorkbuddyCheckedInToday(account: WorkbuddyAccount): boolean {
  if (!account.last_checkin_time) return false;
  const checked = new Date(account.last_checkin_time * 1000);
  const now = new Date();
  return (
    checked.getFullYear() === now.getFullYear() &&
    checked.getMonth() === now.getMonth() &&
    checked.getDate() === now.getDate()
  );
}

/** 按账号的实时状态（签到 + 旅行），字段与后端 get_workbuddy_account_live_status 对齐 */
interface WbLiveStatus {
  // 今日是否已签到（实时查询结果）
  checked_in?: boolean;
  // 签到状态是否查询成功（失败时回退本地 last_checkin_time 判断）
  checked_in_ok?: boolean;
  // 旅行状态（查询失败时为空，按「可旅行」兜底）
  travel?: {
    state: string;
    label: string;
    // 徽标色调：green=可旅行 / blue=旅行中 / gray=旅行结束
    tone: string;
    daily_limit_reached: boolean;
    location_name?: string;
    arrive_at?: number;
  } | null;
}

/** 批量拉取账号实时状态：分批并发避免瞬时打满上游，每批完成即增量回调刷新界面 */
async function fetchWbLiveStatuses(
  ids: string[],
  onBatch: (map: Record<string, WbLiveStatus>) => void,
): Promise<void> {
  const map: Record<string, WbLiveStatus> = {};
  const CHUNK = 4;
  for (let i = 0; i < ids.length; i += CHUNK) {
    const slice = ids.slice(i, i + CHUNK);
    const results = await Promise.all(
      slice.map(async (id) => {
        try {
          const status = await invoke<WbLiveStatus>('get_workbuddy_account_live_status', { accountId: id });
          return [id, status] as const;
        } catch {
          // 单个账号查询失败不阻断其他账号，前端按兜底逻辑展示
          return null;
        }
      }),
    );
    for (const item of results) {
      if (item) map[item[0]] = item[1];
    }
    onBatch({ ...map });
  }
}

/** 按账号实时状态 hook：账号列表变化（含手动刷新账号）后自动重新拉取；拉取期间的新请求合并为一次 */
function useWbAccountLiveStatuses(accounts: WorkbuddyAccount[]): Record<string, WbLiveStatus> {
  const [statuses, setStatuses] = useState<Record<string, WbLiveStatus>>({});
  const runningRef = useRef(false);
  const pendingRef = useRef<WorkbuddyAccount[] | null>(null);

  useEffect(() => {
    pendingRef.current = accounts;
    const run = async () => {
      if (runningRef.current) return;
      runningRef.current = true;
      try {
        // 循环消费最新一次的账号列表；拉取中再次触发的刷新不会丢
        for (let latest = pendingRef.current; latest; latest = pendingRef.current) {
          pendingRef.current = null;
          if (latest.length === 0) {
            setStatuses({});
            continue;
          }
          await fetchWbLiveStatuses(latest.map((a) => a.id), setStatuses);
        }
      } finally {
        runningRef.current = false;
      }
    };
    void run();
  }, [accounts]);

  return statuses;
}

/** 卡片徽标悬停提示所需的时间信息：签到实际时间 + 今日排期时间（按账号） */
interface WbBadgeTimeHints {
  // 今日实际签到时间（"HH:mm:ss"，来自自动签到日志或账号落盘时间；未签到为空）
  checkinTimeText?: string;
  // 今日自动签到排期时间（"HH:mm"，仅自动签到开启且排期属于今天时有值）
  scheduledTimeText?: string;
}

/** 自动签到配置 + 今日执行日志 hook：双通道（window + tauri）监听变更，供卡片徽标悬停展示 */
function useWbAutoCheckinBadgeHints(): { config: WorkbuddyAutoCheckinConfig; logTimes: Record<string, string> } {
  const [config, setConfig] = useState<WorkbuddyAutoCheckinConfig>(() => getWorkbuddyAutoCheckinConfig());
  // 今日自动签到执行时间表：accountId → "HH:mm:ss"（仅成功/已签记录）
  const [logTimes, setLogTimes] = useState<Record<string, string>>({});

  // 配置变更 → 重新拉取（与签到弹窗同款双通道事件）
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    const handleConfigChange = () => {
      void getWorkbuddyAutoCheckinConfigAsync().then((nextConfig) => {
        if (!disposed) setConfig(nextConfig);
      });
    };
    handleConfigChange();
    window.addEventListener(WORKBUDDY_AUTO_CHECKIN_CONFIG_CHANGED_EVENT, handleConfigChange);
    void listen(WORKBUDDY_AUTO_CHECKIN_CONFIG_CHANGED_EVENT, handleConfigChange)
      .then((stop) => {
        if (disposed) stop();
        else unlisten = stop;
      })
      .catch(() => {});
    return () => {
      disposed = true;
      unlisten?.();
      window.removeEventListener(WORKBUDDY_AUTO_CHECKIN_CONFIG_CHANGED_EVENT, handleConfigChange);
    };
  }, []);

  // 签到日志变更 → 重算今日各账号签到时间
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    const load = () => {
      void getWorkbuddyAutoCheckinLogsAsync()
        .then((logs) => {
          if (disposed) return;
          const today = getLocalTodayStr();
          const todayLog = logs.find((record) => record.date === today);
          const times: Record<string, string> = {};
          for (const detail of todayLog?.details ?? []) {
            if (detail.time && (detail.status === 'success' || detail.status === 'already_checked')) {
              times[detail.accountId] = detail.time;
            }
          }
          setLogTimes(times);
        })
        .catch(() => {});
    };
    load();
    window.addEventListener(WORKBUDDY_AUTO_CHECKIN_LOGS_CHANGED_EVENT, load);
    void listen(WORKBUDDY_AUTO_CHECKIN_LOGS_CHANGED_EVENT, load)
      .then((stop) => {
        if (disposed) stop();
        else unlisten = stop;
      })
      .catch(() => {});
    return () => {
      disposed = true;
      unlisten?.();
      window.removeEventListener(WORKBUDDY_AUTO_CHECKIN_LOGS_CHANGED_EVENT, load);
    };
  }, []);

  return { config, logTimes };
}

/** 账号卡片右上角状态徽标：签到状态（按账号实时）+ 旅行状态（按账号实时，短文案），样式对齐 wb-switch-app */
function WbAccountStatusBadges({
  account,
  live,
  hints,
}: {
  account: WorkbuddyAccount;
  live?: WbLiveStatus;
  // 悬停提示用的时间信息（签到实际时间 / 今日排期时间），由页面按自动签到数据计算
  hints?: WbBadgeTimeHints;
}) {
  // 实时签到状态优先；查询失败/未返回时回退本地 last_checkin_time。
  // 文案与配色：可签到=绿色（is-on），已签到=灰色（is-off，表示今日事项已完成）。
  const checkedIn = live?.checked_in_ok ? Boolean(live.checked_in) : isWorkbuddyCheckedInToday(account);
  const travel = live?.travel;
  const travelLabel = travel?.label ?? '可旅行';
  // 徽标色调由后端 tone 决定：green=可旅行 / blue=旅行中 / gray=旅行结束
  const travelClass = travel?.tone === 'blue' ? 'is-blue' : travel?.tone === 'gray' ? 'is-off' : 'is-on';
  // 悬停提示带上地点名，徽标本身保持简短
  const travelTip = travel?.location_name ? `${travelLabel} · ${travel.location_name}` : travelLabel;
  // 签到徽标悬停提示：已签到 → 实际签到时间；可签到 → 今日排期时间（缺数据时回退通用文案）
  const checkinTip = checkedIn
    ? hints?.checkinTimeText
      ? `签到：${hints.checkinTimeText}`
      : '今日已签到，明天再来'
    : hints?.scheduledTimeText
      ? `排期：${hints.scheduledTimeText}`
      : '今日可签到';
  return (
    <>
      <span className={`wb-card-status ${checkedIn ? 'is-off' : 'is-on'}`} title={checkinTip}>
        <CircleCheck size={12} />
        {checkedIn ? '已签到' : '可签到'}
      </span>
      <span
        className={`wb-card-status ${travelClass}`}
        title={travelTip}
      >
        <PlaneTakeoff size={12} />
        {travelLabel}
      </span>
    </>
  );
}

/** 自定义模型下拉：用自绘弹层替代原生 select，避免深色主题下弹层选项白底白字看不清。 */
function ModelSelect({ models, value, onChange }: { models: string[]; value: string; onChange: (value: string) => void }) {
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  // 点击下拉区域外部时自动收起弹层
  useEffect(() => {
    if (!open) return;
    const onPointerDown = (event: MouseEvent) => {
      if (rootRef.current && !rootRef.current.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener('mousedown', onPointerDown);
    return () => document.removeEventListener('mousedown', onPointerDown);
  }, [open]);
  return (
    <div className="wb-model-select" ref={rootRef}>
      <button type="button" className="wb-model-trigger" onClick={() => setOpen((v) => !v)}>
        <span>{value || '选择模型'}</span>
        <ChevronDown size={15} />
      </button>
      {open && (
        <ul className="wb-model-list" role="listbox">
          {models.map((item) => (
            <li key={item}>
              <button
                type="button"
                role="option"
                aria-selected={item === value}
                className={`wb-model-option${item === value ? ' is-active' : ''}`}
                onClick={() => { onChange(item); setOpen(false); }}
              >
                {item}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** wb2api 管理服务真实状态与 OpenAI 接入信息。 */
export function WorkbuddyApiGatewayPanel() {
 const [config,setConfig]=useState<Config|null>(null); const [gateway,setGateway]=useState<Status|null>(null); const [models,setModels]=useState<string[]>([]); const [showKey,setShowKey]=useState(false); const [copied,setCopied]=useState(''); const [message,setMessage]=useState('你好，请用一句话自我介绍。'); const [model,setModel]=useState(''); const [result,setResult]=useState(''); const [loading,setLoading]=useState(false);
 // 本地账号库统计：账号状态只反映 Cockpit Tools 自己的账号，不与 wb2api 网关账号池挂钩
 const localAccounts=useWorkbuddyAccountStore(s=>s.accounts); const localLoading=useWorkbuddyAccountStore(s=>s.loading);
 const localTotal=localAccounts.length; const localAvailable=useMemo(()=>localAccounts.filter(isWorkbuddyAccountReady).length,[localAccounts]);
 const load=async()=>{setLoading(true);try{const [c,s,m]=await Promise.all([fetch(`${ADMIN_BASE}/api/config`).then(r=>r.json()),fetch(`${ADMIN_BASE}/api/status`).then(r=>r.json()),fetch(`${ADMIN_BASE}/api/models`).then(r=>r.json())]);setConfig(c);setGateway(s);const list=(m.data||[]).map((x:{id:string})=>x.id);setModels(list);setModel(v=>v||list[0]||'')}catch(e){setResult(`无法连接管理服务：${e instanceof Error?e.message:String(e)}`)}finally{setLoading(false)}};
 useEffect(()=>{void load()},[]);
 const key=String(config?.config?.api_key||''); const base=DEFAULT_BASE; const lan=String(config?.lan_base_url||''); const copy=async(name:string,text:string)=>{await navigator.clipboard.writeText(text);setCopied(name);setTimeout(()=>setCopied(''),1500)};
 const test=async()=>{setResult('测试中…');try{const content=await invoke<string>('workbuddy_gateway_chat',{model:model||'hy3',message,apiKey:key});setResult(content)}catch(e){setResult(`调用失败：${e instanceof Error?e.message:String(e)}`)}};
 return <section className="workbuddy-api-gateway-panel"><header><h2>OpenAI兼容网关 <span className="wb-status">{gateway?'已连接':'未连接'}</span></h2><button onClick={()=>void load()} title="刷新"><RefreshCw className={loading ? "spin" : ""} size={16}/> </button></header><div className="wb-gateway-card"><div className="wb-row"><strong>Base URL</strong><code>{base}</code><button onClick={()=>void copy('base',base)}>{copied==='base'?<Check size={15}/>:<Copy size={15}/>}复制</button></div>{lan&&<div className="wb-row"><strong>局域网访问</strong><code>{lan}</code><span title="供同一局域网内其他电脑访问">ⓘ</span><button onClick={()=>void copy('lan',lan)}>{copied==='lan'?<Check size={15}/>:<Copy size={15}/>}复制</button></div>}<div className="wb-row"><strong>容器访问</strong><code>http://host.docker.internal:7863/v1</code><span title="Docker 容器内运行的服务（如 sub2api）使用此地址访问宿主机网关">ⓘ</span><button onClick={()=>void copy('docker','http://host.docker.internal:7863/v1')}>{copied==='docker'?<Check size={15}/>:<Copy size={15}/>}复制</button></div><div className="wb-row"><strong>API Key</strong><code>{key?(showKey?key:'•'.repeat(Math.min(key.length,32))):'未配置'}</code><button onClick={()=>setShowKey(v=>!v)}>{showKey?<EyeOff size={15}/>:<Eye size={15}/>}</button>{key&&<button onClick={()=>void copy('key',key)}>{copied==='key'?<Check size={15}/>:<Copy size={15}/>}复制</button>}</div><div className="wb-meta"><span>账号状态</span><b title="按 Cockpit Tools 本地账号库统计：已配置 token 且未过期即视为可用">{localLoading&&localTotal===0?'读取中':`${localAvailable} / ${localTotal} 可用`}</b></div><div className="wb-chat"><ModelSelect models={models} value={model} onChange={setModel}/><input value={message} onChange={e=>setMessage(e.target.value)}/><button onClick={()=>void test()}><Play size={15}/>发送</button></div>{result&&<pre className="wb-result">{result}</pre>}</div><WbPoolAccountLimits /></section>;
}



const WORKBUDDY_FLOW_NOTICE_COLLAPSED_KEY = 'agtools.workbuddy.flow_notice_collapsed';
const WORKBUDDY_CURRENT_ACCOUNT_ID_KEY = 'agtools.workbuddy.current_account_id';

const workbuddyPlatformConfig: CodebuddySuiteAccountsPlatformConfig<WorkbuddyAccount> = {
  pageClassName: 'workbuddy-accounts-page',
  quickSettingsType: 'workbuddy',
  searchPlaceholderKey: 'workbuddy.search',
  searchPlaceholderDefault: '搜索 WorkBuddy 账号...',
  flowNotice: {
    titleKey: 'workbuddy.flowNotice.title',
    titleDefault: 'WorkBuddy 账号管理说明（点击展开/收起）',
    descKey: 'workbuddy.flowNotice.desc',
    descDefault: '切换账号需读取 WorkBuddy 本地认证存储并调用系统凭据服务进行加解密，数据仅在本地处理。',
    permissionKey: 'workbuddy.flowNotice.permission',
    permissionDefault: '权限范围：读取 WorkBuddy 认证数据库，调用系统凭据能力（macOS Keychain / Windows DPAPI / Linux Secret Service）进行解密/回写。',
    networkKey: 'workbuddy.flowNotice.network',
    networkDefault: '网络范围：OAuth 授权登录与 Token 刷新需联网请求 WorkBuddy 服务。不上传本地密钥或凭证。',
  },
  noAccountsKey: 'workbuddy.noAccounts',
  noAccountsDefault: '暂无 WorkBuddy 账号',
  addAccountTitleKey: 'workbuddy.addAccount',
  addAccountTitleDefault: '添加 WorkBuddy 账号',
  oauthDescKey: 'workbuddy.oauthDesc',
  oauthDescDefault: '点击下方按钮将在浏览器中打开 WorkBuddy 授权页面。',
  oauthFeatureCardClassName: 'workbuddy-oauth-feature-card',
  oauthFeatureTitleKey: 'workbuddy.oauthFeature.oauth.title',
  oauthFeatureTitleDefault: '仅授权 IDE 登录信息',
  oauthFeatureItem1Key: 'workbuddy.oauthFeature.oauth.item1',
  oauthFeatureItem1Default: '在浏览器完成 OAuth 后即可添加账号并用于 IDE 切换。',
  oauthFeatureItem2Key: 'workbuddy.oauthFeature.oauth.item2',
  oauthFeatureItem2Default: '授权完成后会自动刷新资源包配额数据。',
  oauthFeatureItem3Key: 'workbuddy.oauthFeature.oauth.item3',
  oauthFeatureItem3Default: '账号卡片将按资源包展示额度、进度和刷新/到期时间。',
  oauthUrlInputPlaceholderKey: 'workbuddy.oauthUrlInputPlaceholder',
  oauthUrlInputPlaceholderDefault: '可手动输入授权地址',
  oauthWaitingKey: 'workbuddy.oauthWaiting',
  oauthWaitingDefault: '等待授权完成...',
  tokenDescKey: 'workbuddy.tokenDesc',
  tokenDescDefault: '粘贴 WorkBuddy 的 access token：',
  importLocalDescKey: 'workbuddy.import.localDesc',
  importLocalDescDefault: '支持从本机 WorkBuddy 客户端或 JSON 文件导入账号数据。',
  importLocalClientKey: 'workbuddy.import.localClient',
  importLocalClientDefault: '从本机 WorkBuddy 导入',
  syncButtonTitle: (t) => `${t('common.shared.import.label', '导入')} ${t('nav.codebuddyCn', 'CodeBuddy CN')}`,
  syncSuccessMessage: (t, count) => t('common.shared.token.importSuccessMsg', '成功导入 {{count}} 个账号', { count }),
  syncFailedMessage: (t, error) => t('common.shared.token.importFailedMsg', '导入失败: {{error}}', { error }),
  runSync: () => syncWorkbuddyToCodebuddyCn(),
  getDisplayEmail: (account) => getWorkbuddyAccountDisplayEmail(account),
  getPlanBadge: (account) => getWorkbuddyPlanBadge(account),
  getUsage: (account) => getWorkbuddyUsage(account),
  getQuotaGroups: (account, t) => getWorkbuddyQuotaCategoryGroups(account, t),
  hasQuotaData: (_account, groups) => groups.some((g) => g.items.length > 0),
  usagePrefix: 'workbuddy',
  quotaPrefix: 'workbuddy',
  tableUsageClassName: 'workbuddy-table-usage',
  CheckinModal: WorkbuddyCheckinModal,
};

export function WorkbuddyAccountsPage() {
  const [activeTab, setActiveTab] = useState<PlatformOverviewTab>('overview');
  const store = useWorkbuddyAccountStore();
  // 按账号实时状态（签到 + 旅行）：账号列表刷新后自动重新拉取
  const liveStatuses = useWbAccountLiveStatuses(store.accounts);
  // 自动签到配置 + 今日执行日志：为卡片徽标悬停提示提供签到时间 / 排期时间
  const { config: autoCheckinConfig, logTimes: autoCheckinLogTimes } = useWbAutoCheckinBadgeHints();
  // 网关账号池并发状态：卡片内联输入框的数据源（uid → 在途/上限）
  const poolLimits = useWbPoolLimits();
  // 平台配置：签到/旅行徽标移到账户名下方的独立一行（card-badge-row，右对齐、
  // 紧贴首行）；FREE 等套餐标签保持在首行账户名右侧不变。
  const platformConfig = useMemo(
    () => ({
      ...workbuddyPlatformConfig,
      cardBadgesOnSecondRow: true,
      renderCardStatusBadges: (account: WorkbuddyAccount) => {
        // 排期时间：仅自动签到开启且该账号今天的排期已生成时提示
        const schedule = autoCheckinConfig.enabled
          ? autoCheckinConfig.accountSchedules?.[account.id]
          : undefined;
        const scheduledTimeText =
          schedule && schedule.scheduledDate === getLocalTodayStr()
            ? formatMinuteOfDay(schedule.scheduledMinute)
            : undefined;
        // 签到时间：今日自动签到日志优先，其次账号落盘的最近签到时间（仅今天展示）
        const checkinTimeText =
          autoCheckinLogTimes[account.id] ?? formatTodayTimestamp(account.last_checkin_time);
        // 徽标行末尾追加内联并发上限输入框（与签到/旅行徽标同行，右对齐）
        const poolKey = account.uid || account.id;
        const pool = poolLimits.map[poolKey];
        return (
          <>
            <WbAccountStatusBadges
              account={account}
              live={liveStatuses[account.id]}
              hints={{ checkinTimeText, scheduledTimeText }}
            />
            <WbCardLimitInput
              uid={poolKey}
              value={pool?.max_in_flight ?? 0}
              inFlight={pool?.in_flight ?? 0}
              onSaved={() => void poolLimits.reload()}
            />
          </>
        );
      },
    }),
    [liveStatuses, autoCheckinConfig, autoCheckinLogTimes, poolLimits.map, poolLimits.reload],
  );

  const page = useProviderAccountsPage<WorkbuddyAccount>({
    platformKey: 'WorkBuddy',
    oauthLogPrefix: 'WorkbuddyOAuth',
    flowNoticeCollapsedKey: WORKBUDDY_FLOW_NOTICE_COLLAPSED_KEY,
    currentAccountIdKey: WORKBUDDY_CURRENT_ACCOUNT_ID_KEY,
    exportFilePrefix: 'workbuddy_accounts',
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
      startLogin: workbuddyService.startWorkbuddyOAuthLogin,
      completeLogin: workbuddyService.completeWorkbuddyOAuthLogin,
      cancelLogin: workbuddyService.cancelWorkbuddyOAuthLogin,
    },
    dataService: {
      importFromJson: workbuddyService.importWorkbuddyFromJson,
      importFromLocal: workbuddyService.importWorkbuddyFromLocal,
      addWithToken: workbuddyService.addWorkbuddyAccountWithToken,
      exportAccounts: workbuddyService.exportWorkbuddyAccounts,
      injectToVSCode: workbuddyService.injectWorkbuddyToVSCode,
      openWebview: workbuddyService.openWorkbuddyWebview,
    },
    getDisplayEmail: (account) => getWorkbuddyAccountDisplayEmail(account),
  });

  const accountsForInstances = useMemo(
    () =>
      [...store.accounts].sort((a, b) => {
        const currentFirstDiff = compareCurrentAccountFirst(a.id, b.id, store.currentAccountId);
        if (currentFirstDiff !== 0) {
          return currentFirstDiff;
        }
        const diff = b.created_at - a.created_at;
        return page.sortDirection === 'desc' ? diff : -diff;
      }),
    [page.sortDirection, store.accounts, store.currentAccountId],
  );

  return (
    <div className={`ghcp-accounts-page ${workbuddyPlatformConfig.pageClassName}`}    >
      <PlatformOverviewTabsHeader
        platform="workbuddy"
        active={activeTab}
        onTabChange={setActiveTab}
        tabs={['overview', 'sessions', 'instances', 'providers', 'tasks']}
      />
      {activeTab === 'tasks' ? (
        <WorkbuddyAutoTasksPanel />
      ) : activeTab === 'providers' ? (
        <WorkbuddyApiGatewayPanel />
      ) : activeTab === 'sessions' ? (
        <CodebuddySessionManager platform="workbuddy" accounts={store.accounts as any} />
      ) : activeTab === 'instances' ? (
        <WorkbuddyInstancesContent accountsForSelect={accountsForInstances} />
      ) : (
        <CodebuddySuiteAccountsSharedView
          accounts={store.accounts}
          loading={store.loading}
          page={page}
          platformConfig={platformConfig}
          onRefreshAccounts={() => { store.fetchAccounts(); }}
        />
      )}
    </div>
  );
}



