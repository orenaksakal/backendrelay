import { describe, expect, it } from 'vitest';
import type { AuthorizedOrigin } from '@/lib/api/endpoints';
import {
  ORIGIN_SERVICES,
  originDisplay,
  originInputLabel,
  originInputPlaceholder,
  originTypeLabel,
  validateOrigin,
} from './origins';

function origin(service: string, params: string[]): AuthorizedOrigin {
  return { id: `origin-${service}`, service, params };
}

describe('redirect origin validation', () => {
  it('accepts a website URL and stores its host', () => {
    expect(validateOrigin('generic', 'https://example.com/login')).toEqual({
      ok: true,
      params: ['example.com'],
    });
    expect(validateOrigin('generic', 'example.com')).toEqual({
      ok: true,
      params: ['example.com'],
    });
  });

  it('accepts localhost only when it has a port', () => {
    expect(validateOrigin('generic', 'localhost:3002')).toEqual({
      ok: true,
      params: ['localhost:3002'],
    });
    expect(validateOrigin('generic', 'localhost')).toEqual({
      ok: false,
      message: 'Invalid URL.',
    });
  });

  it('rejects a malformed website value', () => {
    expect(validateOrigin('generic', 'http://')).toEqual({
      ok: false,
      message: 'Invalid URL.',
    });
    expect(validateOrigin('generic', '   ')).toEqual({
      ok: false,
      message: 'Enter an origin or project name.',
    });
  });

  it('wraps Vercel and Netlify names in their params shapes', () => {
    expect(validateOrigin('vercel', 'todo-preview')).toEqual({
      ok: true,
      params: ['vercel.app', 'todo-preview'],
    });
    expect(validateOrigin('netlify', 'todo-preview')).toEqual({
      ok: true,
      params: ['todo-preview'],
    });
  });

  it('extracts a custom scheme and rejects malformed ones', () => {
    expect(validateOrigin('custom-scheme', 'myapp://callback')).toEqual({
      ok: true,
      params: ['myapp'],
    });
    expect(validateOrigin('custom-scheme', 'not a scheme')).toEqual({
      ok: false,
      message: 'Invalid scheme.',
    });
  });
});

describe('redirect origin display', () => {
  it('renders each service the way the old dashboard did', () => {
    expect(originDisplay(origin('generic', ['example.com']))).toBe(
      'example.com',
    );
    expect(originDisplay(origin('netlify', ['site-name']))).toBe('site-name');
    expect(originDisplay(origin('vercel', ['vercel.app', 'project']))).toBe(
      'project',
    );
    expect(originDisplay(origin('custom-scheme', ['myapp']))).toBe('myapp://');
  });

  it('labels every service and its input', () => {
    expect(ORIGIN_SERVICES).toHaveLength(4);
    expect(originTypeLabel('generic')).toBe('Website');
    expect(originTypeLabel('vercel')).toBe('Vercel previews');
    expect(originTypeLabel('netlify')).toBe('Netlify previews');
    expect(originTypeLabel('custom-scheme')).toBe('App scheme');
    expect(originTypeLabel('future')).toBe('future');
    expect(originInputLabel('generic')).toBe('Origin domain');
    expect(originInputPlaceholder('custom-scheme')).toBe('app-scheme://');
  });
});
