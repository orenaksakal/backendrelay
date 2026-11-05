import type { InstantAppBackup } from '@/lib/types';
export type BackupFilter = 'all' | 'expiring' | 'retained';
export function backupExpiry(backup: InstantAppBackup, now: number) {
  if (!backup.expires_at) return 'retained';
  const expiry = Date.parse(backup.expires_at);
  if (!Number.isFinite(expiry)) return 'unknown';
  return expiry <= now
    ? 'expired'
    : expiry - now <= 7 * 86400000
      ? 'expiring'
      : 'available';
}
export function selectBackups(
  backups: InstantAppBackup[],
  options: { now: number; search: string; filter: BackupFilter; sort: string },
) {
  const text = options.search.toLowerCase().trim();
  return backups
    .filter((b) => {
      const expiry = backupExpiry(b, options.now);
      return (
        expiry !== 'expired' &&
        (options.filter === 'all' || options.filter === expiry) &&
        `${b.id} ${b.description || ''}`.toLowerCase().includes(text)
      );
    })
    .sort((a, b) =>
      options.sort === 'oldest'
        ? Date.parse(a.backup_at) - Date.parse(b.backup_at)
        : options.sort === 'expiry'
          ? (a.expires_at ? Date.parse(a.expires_at) : Infinity) -
            (b.expires_at ? Date.parse(b.expires_at) : Infinity)
          : Date.parse(b.backup_at) - Date.parse(a.backup_at),
    );
}
export function backupSize(bytes: number | null) {
  if (bytes === null || !Number.isFinite(bytes) || bytes < 0) return 'Unknown';
  if (bytes < 1024) return `${bytes} B`;
  const power = Math.min(4, Math.floor(Math.log(bytes) / Math.log(1024)));
  return `${(bytes / 1024 ** power).toLocaleString(undefined, { maximumFractionDigits: 1 })} ${['B', 'KiB', 'MiB', 'GiB', 'TiB'][power]}`;
}
export function backupTime(value: string, utc: boolean) {
  const date = new Date(value);
  if (!Number.isFinite(date.getTime())) return value;
  return utc
    ? date
        .toISOString()
        .replace('T', ' ')
        .replace(/\.\d+Z$/, ' UTC')
    : date.toLocaleString(undefined, {
        dateStyle: 'medium',
        timeStyle: 'short',
      });
}
