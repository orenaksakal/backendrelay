import { describe, expect, it } from 'vitest';
import {
  closedEventSearch,
  eventSearchFor,
  webhookDetailSearchSchema,
} from './url-state';

describe('webhook detail search', () => {
  it('accepts a trimmed event and drops empty values', () => {
    expect(webhookDetailSearchSchema.parse({ event: ' 0/328/1 ' })).toEqual({
      event: '0/328/1',
    });
    expect(webhookDetailSearchSchema.parse({ event: '' })).toEqual({
      event: undefined,
    });
    expect(webhookDetailSearchSchema.parse({})).toEqual({ event: undefined });
  });

  it('opens and closes the delivery sheet', () => {
    const opened = eventSearchFor({}, '0/1/2');
    expect(opened).toEqual({ event: '0/1/2' });
    expect(eventSearchFor(opened, '0/1/3')).toEqual({ event: '0/1/3' });
    expect(closedEventSearch(opened)).toEqual({});
    expect(closedEventSearch({})).toEqual({});
  });
});
