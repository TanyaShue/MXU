import { useAppStore } from '@/stores/appStore';
import { loggers } from '@/utils';
import i18n from '@/i18n';
import type { Instance } from '@/types/interface';

const log = loggers.task;

const STORAGE_KEY_TRIGGERED = 'mxu_schedule_triggeredSlots';
const STORAGE_KEY_RANDOM_TARGETS = 'mxu_schedule_randomTargets';

const CHECK_INTERVAL_MS = 30_000; // 每 30 秒轮询一次（分钟精度下降低到点延迟）
const SLOT_TTL_MS = 48 * 60 * 60 * 1000; // 触发记录保留 48 小时
const DEBOUNCE_MS = 2_000; // 事件触发后 2 秒内去重

export type ScheduleTriggerCallback = (
  instance: Instance,
  policyName: string,
  slotLabel: string,
) => Promise<boolean>;

function formatSlotKey(date: Date): string {
  const y = date.getFullYear();
  const m = String(date.getMonth() + 1).padStart(2, '0');
  const d = String(date.getDate()).padStart(2, '0');
  const h = String(date.getHours()).padStart(2, '0');
  const mi = String(date.getMinutes()).padStart(2, '0');
  return `${y}-${m}-${d}-${h}-${mi}`;
}

function minuteStart(date: Date): Date {
  return new Date(
    date.getFullYear(),
    date.getMonth(),
    date.getDate(),
    date.getHours(),
    date.getMinutes(),
  );
}

function buildTriggeredSlotKey(instanceId: string, slotStr: string): string {
  return `${instanceId}:${slotStr}`;
}

function dateKey(date: Date): string {
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')}`;
}

function getRandomRanges(policy: NonNullable<Instance['schedulePolicies']>[number]) {
  if (policy.randomRanges?.length) return policy.randomRanges;
  if (policy.startTime && policy.endTime) {
    return [{ startTime: policy.startTime, endTime: policy.endTime }];
  }
  return [];
}

function parseMinutes(value: string | undefined): number | null {
  if (!value || !/^([01]\d|2[0-3]):[0-5]\d$/.test(value)) return null;
  const [hours, minutes] = value.split(':').map(Number);
  return hours * 60 + minutes;
}

function normalizeTriggeredSlotKey(key: string): string | null {
  const lastColon = key.lastIndexOf(':');
  if (lastColon <= 0) {
    return null;
  }

  const slotStr = key.substring(lastColon + 1);
  if (!/^\d{4}-\d{2}-\d{2}-\d{2}-\d{2}$/.test(slotStr)) {
    return null;
  }

  const prefix = key.substring(0, lastColon);
  const firstColon = prefix.indexOf(':');
  const instanceId = firstColon === -1 ? prefix : prefix.substring(0, firstColon);

  return instanceId ? buildTriggeredSlotKey(instanceId, slotStr) : null;
}

class ScheduleService {
  private intervalId: ReturnType<typeof setInterval> | null = null;
  private checking = false;
  private triggerFn: ScheduleTriggerCallback | null = null;
  private debounceTimer: ReturnType<typeof setTimeout> | null = null;
  private announcedRandomTargets = new Set<string>();

  private getTriggeredSlots(): Set<string> {
    try {
      const val = localStorage.getItem(STORAGE_KEY_TRIGGERED);
      if (!val) {
        return new Set();
      }

      const rawSlots: unknown = JSON.parse(val);
      if (!Array.isArray(rawSlots)) {
        return new Set();
      }

      const normalized = new Set<string>();
      let changed = false;

      for (const item of rawSlots) {
        if (typeof item !== 'string') {
          changed = true;
          continue;
        }

        if (item.startsWith('random:')) {
          normalized.add(item);
          continue;
        }

        const normalizedKey = normalizeTriggeredSlotKey(item);
        if (!normalizedKey) {
          changed = true;
          continue;
        }

        normalized.add(normalizedKey);
        if (normalizedKey !== item) {
          changed = true;
        }
      }

      if (changed) {
        this.setTriggeredSlots(normalized);
      }

      return normalized;
    } catch {
      return new Set();
    }
  }

  private setTriggeredSlots(slots: Set<string>) {
    localStorage.setItem(STORAGE_KEY_TRIGGERED, JSON.stringify([...slots]));
  }

  private getRandomTargets(): Record<string, number> {
    try {
      const raw = JSON.parse(localStorage.getItem(STORAGE_KEY_RANDOM_TARGETS) || '{}');
      return raw && typeof raw === 'object' ? raw : {};
    } catch {
      return {};
    }
  }

  private setRandomTargets(targets: Record<string, number>) {
    localStorage.setItem(STORAGE_KEY_RANDOM_TARGETS, JSON.stringify(targets));
  }

  private announceRandomTarget(
    targetKey: string,
    instanceId: string,
    policyName: string,
    rangeLabel: string,
    targetMinute: number,
  ) {
    if (this.announcedRandomTargets.has(targetKey)) return;
    this.announcedRandomTargets.add(targetKey);
    const message = i18n.t('logs.messages.scheduleRandomGenerated', {
      policy: policyName,
      range: rangeLabel,
      time: `${String(Math.floor(targetMinute / 60)).padStart(2, '0')}:${String(targetMinute % 60).padStart(2, '0')}`,
    });
    // 启动阶段配置日志可能仍在恢复，延迟写入避免被恢复结果覆盖。
    setTimeout(() => {
      useAppStore.getState().addLog(instanceId, { type: 'info', message });
    }, 1200);
  }

  private cleanupOldSlots() {
    const slots = this.getTriggeredSlots();
    if (slots.size === 0) return;

    const cutoff = Date.now() - SLOT_TTL_MS;
    const cleaned = new Set<string>();

    for (const key of slots) {
      if (key.startsWith('random:')) {
        cleaned.add(key);
        continue;
      }
      // 当前格式: instanceId:YYYY-MM-DD-HH-mm
      const lastColon = key.lastIndexOf(':');
      const slotStr = key.substring(lastColon + 1);
      const [y, mo, d, h, mi] = slotStr.split('-').map(Number);
      const slotTs = new Date(y, mo - 1, d, h, mi).getTime();
      if (slotTs >= cutoff) {
        cleaned.add(key);
      }
    }

    if (cleaned.size !== slots.size) {
      this.setTriggeredSlots(cleaned);
    }
  }

  setTriggerCallback(fn: ScheduleTriggerCallback | null) {
    this.triggerFn = fn;
  }

  start() {
    if (this.intervalId) return;

    log.info('[调度器] 启动，轮询间隔', CHECK_INTERVAL_MS / 1000, '秒');

    this.check();
    this.intervalId = setInterval(() => this.check(), CHECK_INTERVAL_MS);

    document.addEventListener('visibilitychange', this.handleVisibilityChange);
    window.addEventListener('focus', this.handleFocus);
  }

  stop() {
    if (this.intervalId) {
      clearInterval(this.intervalId);
      this.intervalId = null;
    }
    if (this.debounceTimer) {
      clearTimeout(this.debounceTimer);
      this.debounceTimer = null;
    }

    document.removeEventListener('visibilitychange', this.handleVisibilityChange);
    window.removeEventListener('focus', this.handleFocus);

    log.info('[调度器] 已停止');
  }

  private handleVisibilityChange = () => {
    if (document.visibilityState === 'visible') {
      log.info('[调度器] 窗口可见，触发检查');
      this.debouncedCheck();
    }
  };

  private handleFocus = () => {
    log.info('[调度器] 窗口获得焦点，触发检查');
    this.debouncedCheck();
  };

  private debouncedCheck() {
    if (this.debounceTimer) {
      clearTimeout(this.debounceTimer);
    }
    this.debounceTimer = setTimeout(() => {
      this.debounceTimer = null;
      this.check();
    }, DEBOUNCE_MS);
  }

  async check() {
    if (this.checking || !this.triggerFn) return;
    this.checking = true;

    try {
      const now = new Date();
      const currentSlot = minuteStart(now);
      const weekday = currentSlot.getDay();
      const timeStr = `${String(currentSlot.getHours()).padStart(2, '0')}:${String(
        currentSlot.getMinutes(),
      ).padStart(2, '0')}`;
      const slotStr = formatSlotKey(currentSlot);

      this.cleanupOldSlots();
      const triggeredSlots = this.getTriggeredSlots();
      let slotsModified = false;

      // 时间段随机策略：首次进入当天窗口时抽取一次目标分钟，并持久化到当天结束。
      const randomTargets = this.getRandomTargets();
      let randomTargetsModified = false;
      const randomTargetEntries = new Map<
        string,
        { inst: Instance; policy: NonNullable<Instance['schedulePolicies']>[number]; target: Date }
      >();
      for (const inst of useAppStore.getState().instances) {
        for (const policy of inst.schedulePolicies || []) {
          if (
            !policy.enabled ||
            policy.mode !== 'random' ||
            !policy.weekdays.includes(now.getDay())
          )
            continue;
          for (const [rangeIndex, range] of getRandomRanges(policy).entries()) {
            const start = parseMinutes(range.startTime);
            const end = parseMinutes(range.endTime);
            if (start === null || end === null || end < start) continue;
            const todayStart = new Date(
              now.getFullYear(),
              now.getMonth(),
              now.getDate(),
              Math.floor(start / 60),
              start % 60,
            );
            const todayEnd = new Date(
              now.getFullYear(),
              now.getMonth(),
              now.getDate(),
              Math.floor(end / 60),
              end % 60,
            );
            if (now < todayStart || now > todayEnd) continue;
            const targetKey = `${inst.id}:${policy.id}:${dateKey(now)}:${rangeIndex}`;
            let targetMinute = randomTargets[targetKey];
            if (!Number.isFinite(targetMinute)) {
              const lower = Math.max(start, now.getHours() * 60 + now.getMinutes());
              targetMinute = lower + Math.floor(Math.random() * (end - lower + 1));
              randomTargets[targetKey] = targetMinute;
              randomTargetsModified = true;
              log.info(
                `[调度器] 为策略 "${policy.name}" 生成今日第 ${rangeIndex + 1} 个随机时间 ${String(Math.floor(targetMinute / 60)).padStart(2, '0')}:${String(targetMinute % 60).padStart(2, '0')}`,
              );
            }
            this.announceRandomTarget(
              targetKey,
              inst.id,
              policy.name,
              `${range.startTime} - ${range.endTime}`,
              targetMinute,
            );
            const target = new Date(
              now.getFullYear(),
              now.getMonth(),
              now.getDate(),
              Math.floor(targetMinute / 60),
              targetMinute % 60,
            );
            randomTargetEntries.set(targetKey, { inst, policy, target });
          }
        }
      }
      if (randomTargetsModified) this.setRandomTargets(randomTargets);

      for (const [targetKey, { inst, policy, target }] of randomTargetEntries) {
        if (now < target) continue;
        const triggeredKey = `random:${targetKey}`;
        if (triggeredSlots.has(triggeredKey)) continue;
        const freshInst = useAppStore.getState().instances.find((i) => i.id === inst.id);
        if (!freshInst) continue;
        if (freshInst.isRunning) {
          log.info(
            `[调度器] 实例 "${freshInst.name}" 正在运行，跳过随机时间段策略 "${policy.name}"`,
          );
          triggeredSlots.add(triggeredKey);
          slotsModified = true;
          continue;
        }
        triggeredSlots.add(triggeredKey);
        slotsModified = true;
        const targetLabel = `${String(target.getHours()).padStart(2, '0')}:${String(target.getMinutes()).padStart(2, '0')}`;
        try {
          await this.triggerFn(freshInst, policy.name, targetLabel);
        } catch (err) {
          log.error('[调度器] 随机时间段触发失败:', err);
        }
      }

      const { instances } = useAppStore.getState();

      for (const inst of instances) {
        const policies = inst.schedulePolicies || [];

        for (const policy of policies) {
          if (!policy.enabled) continue;
          if (!policy.weekdays.includes(weekday)) continue;
          if (policy.mode === 'random') continue;
          if (!policy.times?.includes(timeStr)) continue;

          const slotKey = buildTriggeredSlotKey(inst.id, slotStr);
          if (triggeredSlots.has(slotKey)) break;

          const freshInst = useAppStore.getState().instances.find((i) => i.id === inst.id);
          if (!freshInst) continue;

          if (freshInst.isRunning) {
            log.info(
              `[调度器] 实例 "${inst.name}" 正在运行，跳过时间槽 ${slotStr} 策略 "${policy.name}"`,
            );
            triggeredSlots.add(slotKey);
            slotsModified = true;
            break;
          }

          log.info(
            `[调度器] 准时触发: 时间槽 ${slotStr}, 实例 "${inst.name}", 策略 "${policy.name}"`,
          );

          triggeredSlots.add(slotKey);
          slotsModified = true;

          try {
            await this.triggerFn(freshInst, policy.name, timeStr);
          } catch (err) {
            log.error(`[调度器] 触发失败:`, err);
          }

          // 每个实例每个时间槽只执行第一个匹配策略
          break;
        }
      }

      if (slotsModified) {
        this.setTriggeredSlots(triggeredSlots);
      }
    } finally {
      this.checking = false;
    }
  }
}

export const scheduleService = new ScheduleService();
