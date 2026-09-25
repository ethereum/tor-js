export type LogLevel = 'trace' | 'debug' | 'info' | 'warn' | 'error';

const LEVEL_ORDER: readonly LogLevel[] = ['trace', 'debug', 'info', 'warn', 'error'];

/** @internal Whether `level` is at or above `min` (e.g. `warn` passes an `info` minimum). */
export function levelEnabled(level: LogLevel, min: LogLevel): boolean {
  return LEVEL_ORDER.indexOf(level) >= LEVEL_ORDER.indexOf(min);
}

// Keys of the one-time warnings already given. Deliberately module state: a
// one-time warning is once per process, however many clients or gateways
// trigger it.
const warnedOnce = new Set<string>();

type RawLog = (level: LogLevel, ...args: unknown[]) => void;

interface LogConstructorParams {
  rawLog?: RawLog;
  /**
   * @internal Where {@link Log.warnOnce} writes, if not `rawLog`. Lets a log
   * that discards everything else still deliver one-time warnings.
   */
  rawLogOnce?: RawLog;
  parentStartTime?: number;
  namePrefix?: string;
}

export class Log {
  private rawLog: RawLog;
  private rawLogOnce: RawLog;
  private parentStartTime: number;
  private namePrefix: string;

  constructor(params: LogConstructorParams = {}) {
    this.parentStartTime = params.parentStartTime ?? Date.now();
    this.namePrefix = params.namePrefix ?? '';
    this.rawLog = params.rawLog ?? this.defaultRawLog.bind(this);
    this.rawLogOnce = params.rawLogOnce ?? this.rawLog;
  }

  child(name: string): Log {
    const newPrefix = this.namePrefix ? `${this.namePrefix}.${name}` : name;
    return new Log({
      rawLog: this.rawLog,
      rawLogOnce: this.rawLogOnce,
      parentStartTime: this.parentStartTime,
      namePrefix: newPrefix,
    });
  }

  trace(...args: unknown[]): void {
    this.log('trace', ...args);
  }

  debug(...args: unknown[]): void {
    this.log('debug', ...args);
  }

  info(...args: unknown[]): void {
    this.log('info', ...args);
  }

  warn(...args: unknown[]): void {
    this.log('warn', ...args);
  }

  error(...args: unknown[]): void {
    this.log('error', ...args);
  }

  /**
   * Warn once per process for `key`; later calls with the same key are no-ops.
   * For warnings a person needs to see, such as a demo gateway, which is why
   * TorClient's otherwise-silent default log still delivers these.
   */
  warnOnce(key: string, ...args: unknown[]): void {
    if (warnedOnce.has(key)) return;
    warnedOnce.add(key);
    this.emit(this.rawLogOnce, 'warn', args);
  }

  /** @internal Create a callback for WASM setLogCallback */
  _makeWasmCallback(): (level: string, target: string, message: string) => void {
    const levels: ReadonlySet<string> = new Set(['trace', 'debug', 'info', 'warn', 'error']);
    return (level: string, target: string, message: string) => {
      if (!levels.has(level)) {
        this.log('error', `unexpected log level from WASM: ${JSON.stringify(level)}`);
        level = 'debug';
      }
      this.child(target).log(level as LogLevel, message);
    };
  }

  private log(level: LogLevel, ...args: unknown[]): void {
    this.emit(this.rawLog, level, args);
  }

  private emit(raw: RawLog, level: LogLevel, args: unknown[]): void {
    const elapsed = Date.now() - this.parentStartTime;
    const timestamp = formatTimestamp(elapsed);
    if (this.namePrefix) {
      raw(level, `[${timestamp}]`, `[${this.namePrefix}]`, ...args);
    } else {
      raw(level, `[${timestamp}]`, ...args);
    }
  }

  private defaultRawLog(level: LogLevel, ...args: unknown[]): void {
    console[level](...args);
  }
}

function formatTimestamp(elapsedMs: number): string {
  const totalSeconds = Math.floor(elapsedMs / 1000);
  const milliseconds = elapsedMs % 1000;
  const ms = String(milliseconds).padStart(3, '0');

  const days = Math.floor(totalSeconds / 86400);
  const hours = Math.floor((totalSeconds % 86400) / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;

  if (days > 0) {
    return `${days}d ${p2(hours)}:${p2(minutes)}:${p2(seconds)}.${ms}`;
  }
  if (hours > 0) {
    return `${p2(hours)}:${p2(minutes)}:${p2(seconds)}.${ms}`;
  }
  if (minutes > 0) {
    return `${p2(minutes)}:${p2(seconds)}.${ms}`;
  }
  return `${p2(seconds)}.${ms}`;
}

function p2(n: number): string {
  return String(n).padStart(2, '0');
}
