import { expect, test } from './fixtures';

/**
 * The catalog is a dev-only route. `build:e2e` sets `VITE_CATALOG=1` so the
 * Playwright preview build includes it.
 */

test.describe('component catalog', () => {
  test('has no accessibility violations in light and dark', async ({
    page,
    checkA11y,
  }) => {
    await page.goto('/_dev/catalog');

    await expect(
      page.getByRole('heading', { level: 1, name: 'Component catalog' }),
    ).toBeVisible();

    // Charts and skeletons animate; wait so the scan sees the final layouts.
    await page.waitForTimeout(800);

    await page.getByRole('button', { name: 'Light' }).click();
    await expect(page.locator('html')).not.toHaveClass(/dark/);
    await checkA11y();

    await page.getByRole('button', { name: 'Dark' }).click();
    await expect(page.locator('html')).toHaveClass(/dark/);
    await checkA11y();
  });

  test('keyboard Tab order reaches every interactive element', async ({
    page,
  }) => {
    await page.goto('/_dev/catalog');
    await expect(
      page.getByRole('heading', { level: 1, name: 'Component catalog' }),
    ).toBeVisible();
    await page.waitForTimeout(800);

    const tabbableCount = await page.evaluate(() => {
      const selector = [
        'a[href]',
        'button',
        'input:not([type="hidden"])',
        'select',
        'textarea',
        'summary',
        '[contenteditable="true"]',
        '[tabindex]',
        '[role="checkbox"]',
        '[role="switch"]',
        '[role="radio"]',
        '[role="tab"]',
      ].join(',');

      const candidates = Array.from(
        document.querySelectorAll<HTMLElement>(selector),
      ).filter((element) => {
        if (element instanceof SVGElement) return false;
        if (element.tabIndex < 0) return false;
        if (element.hasAttribute('disabled')) return false;
        if (element.getAttribute('aria-disabled') === 'true') return false;
        if (element.closest('[inert]')) return false;
        if (element.closest('[aria-hidden="true"]')) return false;
        // The grid and the editor are 2D widgets with their own roving focus;
        // B.4 makes each a single Tab stop. Their inner focusables are checked
        // below as a unit instead of as individual Tab stops.
        if (element.closest('[data-testid="catalog-data-grid"], .cm-editor')) {
          return false;
        }
        if (element.getClientRects().length === 0) return false;
        const style = window.getComputedStyle(element);
        if (style.visibility === 'hidden' || style.display === 'none') {
          return false;
        }
        return true;
      });

      candidates.forEach((element, index) => {
        element.dataset.catalogTabIndex = String(index);
      });
      return candidates.length;
    });

    expect(tabbableCount).toBeGreaterThan(20);

    const reached = new Set<number>();
    let reachedGrid = false;
    let reachedEditor = false;
    // One extra pass after the last element proves focus wraps instead of
    // stopping somewhere in the page.
    for (let index = 0; index < tabbableCount + 15; index += 1) {
      await page.keyboard.press('Tab');
      const focused = await page.evaluate(() => {
        const active = document.activeElement;
        if (!(active instanceof HTMLElement)) return null;
        const marked = active.closest<HTMLElement>('[data-catalog-tab-index]');
        return {
          index: marked ? Number(marked.dataset.catalogTabIndex) : null,
          inGrid: active.closest('[data-testid="catalog-data-grid"]') !== null,
          inEditor: active.closest('.cm-editor') !== null,
        };
      });
      if (!focused) continue;
      if (focused.index !== null) {
        reached.add(focused.index);
      }
      reachedGrid ||= focused.inGrid;
      reachedEditor ||= focused.inEditor;
    }

    expect(reachedGrid).toBe(true);
    expect(reachedEditor).toBe(true);

    const missing = await page.evaluate(
      (seen) => {
        const seenSet = new Set(seen);
        const missingElements: string[] = [];
        document
          .querySelectorAll<HTMLElement>('[data-catalog-tab-index]')
          .forEach((element) => {
            const index = Number(element.dataset.catalogTabIndex);
            if (seenSet.has(index)) return;
            const label =
              element.getAttribute('aria-label') ??
              element.textContent.trim().slice(0, 60);
            missingElements.push(
              `${String(index)}: ${element.tagName.toLowerCase()} "${label}"`,
            );
          });
        return missingElements;
      },
      [...reached],
    );

    expect(missing).toEqual([]);
  });

  test('matches the visual baseline @visual', async ({ page }) => {
    test.skip(
      process.env.PLAYWRIGHT_VISUAL !== '1',
      'Visual baselines are generated and checked in the Playwright container.',
    );

    await page.goto('/_dev/catalog');
    await expect(
      page.getByRole('heading', { level: 1, name: 'Component catalog' }),
    ).toBeVisible();
    await page.waitForTimeout(600);

    await expect(page).toHaveScreenshot('catalog.png', { fullPage: true });
  });
});
