/**
 * Cron expression parsing and next-run calculation.
 *
 * Supports standard five-field expressions (minute hour day-of-month month day-of-week)
 * with `*`, lists, ranges, steps, month names and weekday names. Evaluation is done in a
 * caller-supplied IANA time zone (UTC by default).
 *
 * DST policy: schedules are wall-clock based. A local time that does not exist (spring
 * forward) is skipped, and an ambiguous local time (fall back) runs only once, at its
 * first occurrence.
 */

export class CronError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "CronError";
  }
}

export interface CronFields {
  readonly minute: readonly number[];
  readonly hour: readonly number[];
  readonly dayOfMonth: readonly number[];
  readonly month: readonly number[];
  readonly dayOfWeek: readonly number[];
  /** True when the raw field starts with `*`; the other day field is then authoritative. */
  readonly dayOfMonthWildcard: boolean;
  readonly dayOfWeekWildcard: boolean;
}

interface FieldSpec {
  readonly min: number;
  readonly max: number;
  readonly names?: Readonly<Record<string, number>>;
}

const MONTH_NAMES: Readonly<Record<string, number>> = {
  JAN: 1,
  FEB: 2,
  MAR: 3,
  APR: 4,
  MAY: 5,
  JUN: 6,
  JUL: 7,
  AUG: 8,
  SEP: 9,
  OCT: 10,
  NOV: 11,
  DEC: 12,
};

const DAY_NAMES: Readonly<Record<string, number>> = {
  SUN: 0,
  MON: 1,
  TUE: 2,
  WED: 3,
  THU: 4,
  FRI: 5,
  SAT: 6,
};

const MINUTE: FieldSpec = { min: 0, max: 59 };
const HOUR: FieldSpec = { min: 0, max: 23 };
const DAY_OF_MONTH: FieldSpec = { min: 1, max: 31 };
const MONTH: FieldSpec = { min: 1, max: 12, names: MONTH_NAMES };
const DAY_OF_WEEK: FieldSpec = { min: 0, max: 7, names: DAY_NAMES };

const WEEKDAY_INDEX: Readonly<Record<string, number>> = {
  Sun: 0,
  Mon: 1,
  Tue: 2,
  Wed: 3,
  Thu: 4,
  Fri: 5,
  Sat: 6,
};

const MAX_SEARCH_DAYS = 366 * 8;
const DAY_MS = 24 * 60 * 60 * 1000;

function parseValue(text: string, spec: FieldSpec, label: string): number {
  const trimmed = text.trim();
  if (trimmed === "") {
    throw new CronError(`${label}: empty value in "${text}"`);
  }
  if (spec.names !== undefined) {
    const named = spec.names[trimmed.toUpperCase()];
    if (named !== undefined) {
      return named;
    }
  }
  if (!/^\d+$/.test(trimmed)) {
    throw new CronError(`${label}: invalid value "${text}"`);
  }
  const value = Number(trimmed);
  if (value < spec.min || value > spec.max) {
    throw new CronError(
      `${label}: value ${value} is outside ${spec.min}-${spec.max}`,
    );
  }
  return value;
}

function parseStep(text: string, label: string): number {
  if (!/^\d+$/.test(text)) {
    throw new CronError(`${label}: invalid step "${text}"`);
  }
  const step = Number(text);
  if (step < 1) {
    throw new CronError(`${label}: step must be at least 1`);
  }
  return step;
}

function parseField(raw: string, spec: FieldSpec, label: string): number[] {
  const values = new Set<number>();

  for (const part of raw.split(",")) {
    if (part === "") {
      throw new CronError(`${label}: empty list element in "${raw}"`);
    }
    const pieces = part.split("/");
    if (pieces.length > 2) {
      throw new CronError(`${label}: invalid term "${part}"`);
    }
    const [base, stepText] = pieces;
    const step = stepText === undefined ? 1 : parseStep(stepText, label);

    let start: number;
    let end: number;
    if (base === "*") {
      start = spec.min;
      end = spec.max;
    } else if (base.includes("-")) {
      const bounds = base.split("-");
      if (bounds.length !== 2) {
        throw new CronError(`${label}: invalid range "${base}"`);
      }
      start = parseValue(bounds[0], spec, label);
      end = parseValue(bounds[1], spec, label);
      if (start > end) {
        throw new CronError(`${label}: range "${base}" is reversed`);
      }
    } else {
      start = parseValue(base, spec, label);
      end = stepText === undefined ? start : spec.max;
    }

    for (let value = start; value <= end; value += step) {
      values.add(value);
    }
  }

  return [...values].sort((a, b) => a - b);
}

/** Parse and validate a five-field cron expression. */
export function parseCron(expression: string): CronFields {
  if (typeof expression !== "string") {
    throw new CronError("cron expression must be a string");
  }
  const fields = expression.trim().split(/\s+/);
  if (fields.length !== 5) {
    throw new CronError(
      `expected 5 fields, got ${fields.length} in "${expression}"`,
    );
  }

  const [minute, hour, dayOfMonth, month, dayOfWeek] = fields;
  const weekdays = parseField(dayOfWeek, DAY_OF_WEEK, "day-of-week").map(
    (value) => (value === 7 ? 0 : value),
  );

  return {
    minute: parseField(minute, MINUTE, "minute"),
    hour: parseField(hour, HOUR, "hour"),
    dayOfMonth: parseField(dayOfMonth, DAY_OF_MONTH, "day-of-month"),
    month: parseField(month, MONTH, "month"),
    dayOfWeek: [...new Set(weekdays)].sort((a, b) => a - b),
    dayOfMonthWildcard: dayOfMonth.startsWith("*"),
    dayOfWeekWildcard: dayOfWeek.startsWith("*"),
  };
}

interface ZonedParts {
  readonly year: number;
  readonly month: number;
  readonly day: number;
  readonly hour: number;
  readonly minute: number;
  readonly second: number;
  readonly weekday: number;
}

const formatters = new Map<string, Intl.DateTimeFormat>();

function getFormatter(timeZone: string): Intl.DateTimeFormat {
  const cached = formatters.get(timeZone);
  if (cached !== undefined) {
    return cached;
  }
  let formatter: Intl.DateTimeFormat;
  try {
    formatter = new Intl.DateTimeFormat("en-US", {
      timeZone,
      hourCycle: "h23",
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
      weekday: "short",
    });
  } catch {
    throw new CronError(`invalid time zone "${timeZone}"`);
  }
  formatters.set(timeZone, formatter);
  return formatter;
}

function getZonedParts(date: Date, timeZone: string): ZonedParts {
  const values: Record<string, string> = {};
  for (const part of getFormatter(timeZone).formatToParts(date)) {
    if (part.type !== "literal") {
      values[part.type] = part.value;
    }
  }

  const weekday = WEEKDAY_INDEX[values.weekday];
  if (weekday === undefined) {
    throw new CronError(`unexpected weekday "${values.weekday}"`);
  }

  return {
    year: Number(values.year),
    month: Number(values.month),
    day: Number(values.day),
    hour: Number(values.hour),
    minute: Number(values.minute),
    second: Number(values.second),
    weekday,
  };
}

function offsetMs(date: Date, timeZone: string): number {
  const parts = getZonedParts(date, timeZone);
  const asUtc = Date.UTC(
    parts.year,
    parts.month - 1,
    parts.day,
    parts.hour,
    parts.minute,
    parts.second,
  );
  const truncated = Math.floor(date.getTime() / 1000) * 1000;
  return asUtc - truncated;
}

/**
 * Resolve a wall-clock time to its UTC instants. A normal time yields one instant,
 * an ambiguous time (fall back) yields two ordered earliest-first, and a time
 * skipped by a forward transition yields none.
 */
function resolveWallTime(
  year: number,
  month: number,
  day: number,
  hour: number,
  minute: number,
  timeZone: string,
): Date[] {
  const wall = Date.UTC(year, month - 1, day, hour, minute, 0, 0);
  const offsets = new Set([
    offsetMs(new Date(wall - DAY_MS), timeZone),
    offsetMs(new Date(wall + DAY_MS), timeZone),
  ]);

  const instants = new Map<number, Date>();
  for (const offset of offsets) {
    const candidate = new Date(wall - offset);
    if (
      isSameWallTime(
        getZonedParts(candidate, timeZone),
        year,
        month,
        day,
        hour,
        minute,
      )
    ) {
      instants.set(candidate.getTime(), candidate);
    }
  }
  return [...instants.values()].sort((a, b) => a.getTime() - b.getTime());
}

function isSameWallTime(
  parts: ZonedParts,
  year: number,
  month: number,
  day: number,
  hour: number,
  minute: number,
): boolean {
  return (
    parts.year === year &&
    parts.month === month &&
    parts.day === day &&
    parts.hour === hour &&
    parts.minute === minute
  );
}

export interface NextRunOptions {
  /** Instant to search strictly after. Defaults to now. */
  readonly from?: Date;
  /** IANA time zone used for wall-clock matching. Defaults to UTC. */
  readonly timeZone?: string;
}

/**
 * Return the first matching instant strictly after `from`.
 *
 * Throws `CronError` for invalid expressions, invalid time zones, invalid `from`
 * values, and schedules with no occurrence within eight years.
 */
export function nextRun(
  expression: string,
  options: NextRunOptions = {},
): Date {
  const fields = parseCron(expression);
  const timeZone = options.timeZone ?? "UTC";
  const from = options.from ?? new Date();

  if (!(from instanceof Date) || Number.isNaN(from.getTime())) {
    throw new CronError("from must be a valid Date");
  }
  getFormatter(timeZone);

  const start = getZonedParts(from, timeZone);
  let year = start.year;
  let month = start.month;
  let day = start.day;

  for (let dayOffset = 0; dayOffset < MAX_SEARCH_DAYS; dayOffset += 1) {
    const dayNumber = new Date(Date.UTC(year, month - 1, day));
    const weekday = dayNumber.getUTCDay();
    const domMatches = fields.dayOfMonth.includes(day);
    const dowMatches = fields.dayOfWeek.includes(weekday);

    const bothRestricted =
      !fields.dayOfMonthWildcard && !fields.dayOfWeekWildcard;
    // Vixie semantics: two restricted day fields OR together; if either field
    // starts with `*`, both must match (a stepped `*/n` still filters).
    const dayMatches = bothRestricted
      ? domMatches || dowMatches
      : domMatches && dowMatches;

    if (fields.month.includes(month) && dayMatches) {
      for (const hour of fields.hour) {
        for (const minute of fields.minute) {
          const candidates = resolveWallTime(
            year,
            month,
            day,
            hour,
            minute,
            timeZone,
          );
          const first = candidates[0];
          // An ambiguous time contributes only its first occurrence. If `from`
          // is already past it, the repeated hour is skipped rather than run twice.
          if (first !== undefined && first.getTime() > from.getTime()) {
            return first;
          }
        }
      }
    }

    const following = new Date(Date.UTC(year, month - 1, day + 1));
    year = following.getUTCFullYear();
    month = following.getUTCMonth() + 1;
    day = following.getUTCDate();
  }

  throw new CronError(
    `no matching time for "${expression}" within eight years`,
  );
}
