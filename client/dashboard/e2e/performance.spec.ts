import type { Page } from '@playwright/test';
import { expect, test } from './fixtures';

/**
 * D9.3 performance budget: the Explorer keeps scrolling smooth with a full
 * 10k-row page. The stub serves that page size, which exercises AG Grid's row
 * virtualization instead of the two-row fixtures every other spec uses.
 *
 * The trace is always captured for this spec so a failure can be inspected;
 * the assertion itself comes from the browser's Long Tasks API.
 */

const SESSION = JSON.stringify({
  token: 'e2e-session-token',
  user: { id: 'user-1', email: 'owner@example.com' },
});

const ROWS = 10_000;
const PAGE_SIZE = 10_000;
const LONG_TASK_LIMIT_MS = 200;

// File scope: Playwright rejects `use()` inside a describe group.
test.use({ trace: 'on' });

interface LongTask {
  startTime: number;
  duration: number;
}

async function seedPerformanceSession(page: Page): Promise<void> {
  await page.addInitScript(
    ({ session, rows }) => {
      window.localStorage.setItem('br.session', session);
      window.localStorage.setItem('br.mockInstant', '1');
      window.localStorage.setItem('br.mockInstantRows', String(rows));
    },
    { session: SESSION, rows: ROWS },
  );
}

/** Scroll the grid's own viewport from the top to `fraction` of its height. */
async function scrollGrid(page: Page, fraction: number): Promise<number> {
  return page.evaluate(async (target) => {
    const viewport = document.querySelector<HTMLElement>('.ag-grid-viewport');
    if (viewport === null) {
      throw new Error('The Explorer grid viewport is missing.');
    }
    const max = Math.max(0, viewport.scrollHeight - viewport.clientHeight);
    const steps = 40;
    for (let step = 1; step <= steps; step += 1) {
      viewport.scrollTop = (max * target * step) / steps;
      await new Promise<void>((resolve) => {
        requestAnimationFrame(() => {
          resolve();
        });
      });
    }
    return viewport.scrollTop;
  }, fraction);
}

test.describe('performance', () => {
  test('scrolls 10k Explorer rows without a long task over 200ms', async ({
    page,
  }) => {
    const testInfo = test.info();
    test.setTimeout(180_000);
    await seedPerformanceSession(page);

    await page.goto(`/apps/app-1/explorer?ns=todos&size=${String(PAGE_SIZE)}`);
    await expect(
      page.getByRole('heading', { level: 1, name: 'Explorer' }),
    ).toBeVisible();
    await expect(page.locator('.ag-grid-viewport')).toBeVisible({
      timeout: 30_000,
    });
    await expect(page.locator('.ag-row').first()).toContainText('Mock todo 1', {
      timeout: 30_000,
    });

    // Warm up the grid's renderer before recording; only the scroll matters.
    await scrollGrid(page, 0.25);

    await page.evaluate(() => {
      const tasks: LongTask[] = [];
      const typedWindow = window as unknown as {
        __brLongTasks: LongTask[];
      };
      typedWindow.__brLongTasks = tasks;
      new PerformanceObserver((list) => {
        for (const entry of list.getEntries()) {
          tasks.push({ startTime: entry.startTime, duration: entry.duration });
        }
      }).observe({ entryTypes: ['longtask'] });
    });

    const scrolledTo = await scrollGrid(page, 1);
    expect(scrolledTo).toBeGreaterThan(0);
    // Let the final frame's tasks land before reading the observer.
    await page.waitForTimeout(250);

    const tasks = await page.evaluate(() => {
      const typedWindow = window as unknown as {
        __brLongTasks?: LongTask[];
      };
      return [...(typedWindow.__brLongTasks ?? [])];
    });
    const longest = tasks.reduce(
      (max, task) => Math.max(max, task.duration),
      0,
    );

    await testInfo.attach('long-tasks.json', {
      body: JSON.stringify({ rows: ROWS, longest, tasks }, null, 2),
      contentType: 'application/json',
    });

    expect(
      longest,
      `longest long task while scrolling ${String(ROWS)} rows`,
    ).toBeLessThan(LONG_TASK_LIMIT_MS);
  });
});
