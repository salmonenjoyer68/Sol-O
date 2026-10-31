import { describe, expect, it } from "vitest";

import { CronError, nextRun, parseCron } from "../src/cron";

function utc(iso: string): Date {
  return new Date(iso);
}

describe("parseCron", () => {
  it("parses wildcards, lists, ranges and steps", () => {
    const fields = parseCron("*/15 9-17 1,15 JAN-MAR MON-FRI");
    expect(fields.minute).toEqual([0, 15, 30, 45]);
    expect(fields.hour).toEqual([9, 10, 11, 12, 13, 14, 15, 16, 17]);
    expect(fields.dayOfMonth).toEqual([1, 15]);
    expect(fields.month).toEqual([1, 2, 3]);
    expect(fields.dayOfWeek).toEqual([1, 2, 3, 4, 5]);
    expect(fields.dayOfMonthWildcard).toBe(false);
    expect(fields.dayOfWeekWildcard).toBe(false);
  });

  it("treats 7 as Sunday and deduplicates weekday values", () => {
    const fields = parseCron("0 0 * * 0,7");
    expect(fields.dayOfWeek).toEqual([0]);
    expect(fields.dayOfWeekWildcard).toBe(false);
  });

  it("marks a bare asterisk day field as a wildcard", () => {
    const fields = parseCron("0 0 * * *");
    expect(fields.dayOfMonthWildcard).toBe(true);
    expect(fields.dayOfWeekWildcard).toBe(true);
  });

  it("treats a leading asterisk day field as a wildcard", () => {
    const fields = parseCron("0 0 */1 * MON");
    expect(fields.dayOfMonthWildcard).toBe(true);
    expect(fields.dayOfWeekWildcard).toBe(false);
  });

  it("expands an open-ended step from a start value", () => {
    expect(parseCron("0 10/5 * * *").hour).toEqual([10, 15, 20]);
  });

  it.each([
    ["", "expected 5 fields"],
    ["* * * *", "expected 5 fields"],
    ["0 0 0 * *", "outside 1-31"],
    ["60 * * * *", "outside 0-59"],
    ["* * * * 8", "outside 0-7"],
    ["*/0 * * * *", "at least 1"],
    ["5-1 * * * *", "reversed"],
    ["* * * FOO *", "invalid value"],
    ["* * * * MON--FRI", "invalid range"],
    ["1,,2 * * * *", "empty list element"],
  ])("rejects %j with %s", (expression, message) => {
    expect(() => parseCron(expression)).toThrow(CronError);
    expect(() => parseCron(expression)).toThrow(message);
  });
});

describe("nextRun", () => {
  it("finds the next matching minute strictly after the reference", () => {
    expect(nextRun("*/15 * * * *", { from: utc("2026-01-01T00:07:00Z") })).toEqual(
      utc("2026-01-01T00:15:00Z"),
    );
    expect(nextRun("*/15 * * * *", { from: utc("2026-01-01T00:15:00Z") })).toEqual(
      utc("2026-01-01T00:30:00Z"),
    );
  });

  it("rolls to the next day for a daily schedule", () => {
    expect(nextRun("0 0 * * *", { from: utc("2026-01-01T00:00:00Z") })).toEqual(
      utc("2026-01-02T00:00:00Z"),
    );
  });

  it("uses the requested time zone", () => {
    expect(
      nextRun("0 9 * * *", {
        from: utc("2026-01-01T00:00:00Z"),
        timeZone: "America/New_York",
      }),
    ).toEqual(utc("2026-01-01T14:00:00Z"));
  });

  it("skips a local time that does not exist on the spring-forward day", () => {
    expect(
      nextRun("30 2 * * *", {
        from: utc("2026-03-08T00:00:00Z"),
        timeZone: "America/New_York",
      }),
    ).toEqual(utc("2026-03-09T06:30:00Z"));
  });

  it("runs an ambiguous fall-back time only at its first occurrence", () => {
    expect(
      nextRun("30 1 * * *", {
        from: utc("2026-11-01T04:00:00Z"),
        timeZone: "America/New_York",
      }),
    ).toEqual(utc("2026-11-01T05:30:00Z"));
    expect(
      nextRun("30 1 * * *", {
        from: utc("2026-11-01T05:45:00Z"),
        timeZone: "America/New_York",
      }),
    ).toEqual(utc("2026-11-02T06:30:00Z"));
  });

  it.each([
    ["Europe/Berlin", "30 2 * * *", "2026-10-25T00:00:00Z", "2026-10-25T00:30:00Z"],
    ["Europe/London", "30 1 * * *", "2026-10-25T00:00:00Z", "2026-10-25T00:30:00Z"],
    ["Australia/Sydney", "30 2 * * *", "2026-04-04T00:00:00Z", "2026-04-04T15:30:00Z"],
    ["Australia/Lord_Howe", "45 1 * * *", "2026-04-04T00:00:00Z", "2026-04-04T14:45:00Z"],
  ])(
    "resolves an ambiguous %s time to its first occurrence",
    (timeZone, expression, from, expected) => {
      expect(nextRun(expression, { from: utc(from), timeZone })).toEqual(
        utc(expected),
      );
    },
  );

  it("skips the repeated hour once its first occurrence has passed", () => {
    expect(
      nextRun("30 2 * * *", {
        from: utc("2026-10-25T01:15:00Z"),
        timeZone: "Europe/Berlin",
      }),
    ).toEqual(utc("2026-10-26T01:30:00Z"));
  });

  it("does not skip minutes across a fall-back transition", () => {
    expect(
      nextRun("* * * * *", {
        from: utc("2026-10-25T00:50:00Z"),
        timeZone: "Europe/Berlin",
      }),
    ).toEqual(utc("2026-10-25T00:51:00Z"));
  });

  it("ORs restricted day-of-month and day-of-week fields", () => {
    expect(
      nextRun("0 0 13 * 5", { from: utc("2026-01-01T00:00:00Z") }),
    ).toEqual(utc("2026-01-02T00:00:00Z"));
  });

  it("defers to the weekday when day-of-month starts with an asterisk", () => {
    expect(
      nextRun("0 0 */1 * MON", { from: utc("2026-01-06T00:00:00Z") }),
    ).toEqual(utc("2026-01-12T00:00:00Z"));
  });

  it.each([
    ["0 0 */2 * *", "2026-01-03T00:00:00Z"],
    ["0 0 * * */2", "2026-01-03T00:00:00Z"],
  ])("applies a step to a starred day field (%s)", (expression, expected) => {
    expect(
      nextRun(expression, { from: utc("2026-01-01T00:00:00Z") }),
    ).toEqual(utc(expected));
  });

  it("matches Sunday for weekday 7", () => {
    expect(nextRun("0 0 * * 7", { from: utc("2026-01-01T00:00:00Z") })).toEqual(
      utc("2026-01-04T00:00:00Z"),
    );
  });

  it("accepts month and weekday names", () => {
    expect(
      nextRun("0 0 1 JAN *", { from: utc("2026-01-01T00:00:00Z") }),
    ).toEqual(utc("2027-01-01T00:00:00Z"));
    expect(
      nextRun("0 12 * * MON", { from: utc("2026-01-01T00:00:00Z") }),
    ).toEqual(utc("2026-01-05T12:00:00Z"));
  });

  it("throws when a schedule can never occur", () => {
    expect(() =>
      nextRun("0 0 31 2 *", { from: utc("2026-01-01T00:00:00Z") }),
    ).toThrow("no matching time");
  });

  it("rejects an invalid time zone and an invalid reference date", () => {
    expect(() =>
      nextRun("* * * * *", {
        from: utc("2026-01-01T00:00:00Z"),
        timeZone: "Mars/Olympus_Mons",
      }),
    ).toThrow("invalid time zone");
    expect(() => nextRun("* * * * *", { from: new Date("nope") })).toThrow(
      "from must be a valid Date",
    );
    expect(() =>
      nextRun("* * * * *", {
        from: "2026-01-01" as unknown as Date,
      }),
    ).toThrow(CronError);
  });
});
