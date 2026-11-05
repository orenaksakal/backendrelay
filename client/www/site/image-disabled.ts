import type { APIRoute } from 'astro';

// Keep Astro's injected image route inert in every hosting mode and HTTP method.
export const ALL: APIRoute = () =>
  new Response('Not found', {
    status: 404,
    headers: {
      'Content-Type': 'text/plain; charset=utf-8',
      'Cache-Control': 'no-store',
      'X-Content-Type-Options': 'nosniff',
    },
  });
