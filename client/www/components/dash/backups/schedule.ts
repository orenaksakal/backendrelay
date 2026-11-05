// Mirrors the server's five-field UTC cron syntax. The server remains authoritative.
export function nextBackupOccurrence(cron: string, after = Date.now()): string {
  const fields = cron.trim().split(/\s+/);
  if (fields.length !== 5 || cron.length > 128)
    throw new Error('Enter a five-field cron expression.');
  const ranges = [
    [0, 59],
    [0, 23],
    [1, 31],
    [1, 12],
    [0, 6],
  ];
  const sets = fields.map((field, index) => {
    const [min, max] = ranges[index];
    const values = new Set<number>();
    for (const part of field.split(',')) {
      if (!/^(\*|\d+(-\d+)?)(\/\d+)?$/.test(part))
        throw new Error(
          'Use numbers, *, ranges, lists, or / steps in cron fields.',
        );
      const [base, stepText] = part.split('/');
      const step = stepText === undefined ? 1 : Number(stepText);
      const [start, end] =
        base === '*'
          ? [min, max]
          : base.includes('-')
            ? base.split('-').map(Number)
            : [Number(base), Number(base)];
      if (
        start < min ||
        end > max ||
        start > end ||
        !Number.isSafeInteger(step) ||
        step < 1
      )
        throw new Error(
          `Cron field ${index + 1} must use values ${min}–${max} and a positive step.`,
        );
      for (let n = start; n <= end; n += step) values.add(n);
    }
    return values;
  });
  const start = Math.floor(after / 60000) * 60000 + 60000;
  for (let i = 0; i < 400 * 24 * 60; i++) {
    const date = new Date(start + i * 60000);
    const day = sets[2].has(date.getUTCDate());
    const weekday = sets[4].has(date.getUTCDay());
    const calendar =
      fields[2] === '*' || fields[4] === '*' ? day && weekday : day || weekday;
    if (
      sets[0].has(date.getUTCMinutes()) &&
      sets[1].has(date.getUTCHours()) &&
      sets[3].has(date.getUTCMonth() + 1) &&
      calendar
    )
      return date.toISOString();
  }
  throw new Error('This cron expression has no occurrence within 400 days.');
}
