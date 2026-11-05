// No provider access or browser navigation: verify installed engines can launch.
import {
  chromium,
  firefox,
  webkit,
} from '../client/packages/core/node_modules/playwright/index.mjs';
let failed = false;
for (const [name, engine] of Object.entries({ chromium, firefox, webkit })) {
  let browser;
  try {
    browser = process.env.INSTANT_PLAYWRIGHT_WS_ENDPOINT
      ? await engine.connect(process.env.INSTANT_PLAYWRIGHT_WS_ENDPOINT, {
          timeout: 15_000,
        })
      : await engine.launch({ headless: true, timeout: 15_000 });
    console.log(`${name}: passed`);
  } catch {
    failed = true;
    console.log(`${name}: blocked (engine or host dependencies unavailable)`);
  } finally {
    await browser?.close();
  }
}
process.exitCode = failed ? 1 : 0;
