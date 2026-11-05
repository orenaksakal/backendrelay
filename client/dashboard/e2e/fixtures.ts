import AxeBuilder from '@axe-core/playwright';
import { test as base, expect } from '@playwright/test';

export const test = base.extend<{
  checkA11y: (exclude?: string[]) => Promise<void>;
}>({
  checkA11y: async ({ page }, provide, testInfo) => {
    await provide(async (exclude = []) => {
      // Let closing dialogs, toasts and theme transitions finish so axe reads
      // the settled layout and colours rather than an in-between frame.
      await page.evaluate(
        () =>
          new Promise<void>((resolve) => {
            requestAnimationFrame(() => {
              requestAnimationFrame(() => {
                resolve();
              });
            });
          }),
      );
      await page.waitForTimeout(200);
      let axe = new AxeBuilder({ page })
        .options({ rules: { 'target-size': { enabled: true } } })
        .withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa']);
      for (const selector of exclude) {
        axe = axe.exclude(selector);
      }
      const { violations } = await axe.analyze();
      await testInfo.attach('axe', {
        body: JSON.stringify(violations, null, 2),
        contentType: 'application/json',
      });
      expect(violations).toEqual([]);
    });
  },
});

export { expect };
