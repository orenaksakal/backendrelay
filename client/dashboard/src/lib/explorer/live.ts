/**
 * Live list behaviour (B.5.5): updates land in place, but a list that is
 * sorted or filtered never reorders under the cursor — it offers a refresh
 * instead — and a burst above ten updates per second pauses live data.
 */

export const LIVE_UPDATE_WINDOW_MS = 1000;
export const LIVE_UPDATE_LIMIT = 10;

/** Keeps the updates that are still inside the one-second window, plus `now`. */
export function recordUpdate(
  timestamps: ReadonlyArray<number>,
  now: number,
): number[] {
  return [
    ...timestamps.filter((stamp) => now - stamp < LIVE_UPDATE_WINDOW_MS),
    now,
  ];
}

/** More than ten updates inside the window means the view is too busy to follow. */
export function isHighVolume(timestamps: ReadonlyArray<number>): boolean {
  return timestamps.length > LIVE_UPDATE_LIMIT;
}

/**
 * A sorted or filtered list must not move rows under the cursor, so its live
 * results wait behind the Refresh button instead of applying immediately.
 */
export function holdsLiveUpdates(
  hasSort: boolean,
  hasFilters: boolean,
): boolean {
  return hasSort || hasFilters;
}
