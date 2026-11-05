import { describe, expect, it } from 'vitest';
import {
  LIVE_UPDATE_LIMIT,
  LIVE_UPDATE_WINDOW_MS,
  holdsLiveUpdates,
  isHighVolume,
  recordUpdate,
} from './live';

describe('explorer live updates', () => {
  it('keeps only updates inside the one-second window', () => {
    const now = 10_000;
    expect(
      recordUpdate([now - LIVE_UPDATE_WINDOW_MS - 1, now - 10], now),
    ).toEqual([now - 10, now]);
  });

  it('flags a burst above the limit', () => {
    const stamps = Array.from(
      { length: LIVE_UPDATE_LIMIT },
      (_, index) => index,
    );
    expect(isHighVolume(stamps)).toBe(false);
    expect(isHighVolume([...stamps, LIVE_UPDATE_LIMIT])).toBe(true);
  });

  it('holds updates for sorted or filtered lists', () => {
    expect(holdsLiveUpdates(false, false)).toBe(false);
    expect(holdsLiveUpdates(true, false)).toBe(true);
    expect(holdsLiveUpdates(false, true)).toBe(true);
  });
});
