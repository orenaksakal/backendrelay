const { chromium } = require('../../packages/core/node_modules/playwright');
(async () => {
  const b = await chromium.launch({ headless: true });
  try {
    const p = await b.newPage();
    p.setDefaultTimeout(10000);
    for (const flow of ['application', 'organization', 'onboarding']) {
      await p.goto('http://127.0.0.1:3015/dash?preview=creation');
      await p.getByRole('button', { name: 'Preview controls' }).click();
      await p.getByLabel(/^Flow/).selectOption(flow);
      const kind = flow === 'organization' ? 'Organization' : 'Application';
      await p
        .getByLabel(`${kind} name`, { exact: true })
        .fill(`Regression ${flow}`);
      await p
        .getByRole('button', {
          name: `Create ${kind.toLowerCase()}`,
          exact: true,
        })
        .click();
      await p.getByText(/Write attempts: 1/).waitFor();
      await p
        .getByRole('button', {
          name: `Open ${kind.toLowerCase()}`,
          exact: true,
        })
        .click();
      await p
        .locator('code')
        .filter({ hasText: /\/dash(?:\/org)?\?.*(?:app|org)=/ })
        .first()
        .waitFor();
      console.log('PASS', flow, 'created once and opened');
    }
    await p.goto('http://127.0.0.1:3015/dash?preview=creation');
    await p.getByRole('button', { name: 'Preview controls' }).click();
    await p.getByLabel(/^Requests/).selectOption('lost-response');
    await p.getByLabel('Application name', { exact: true }).fill('Recoverable');
    await p
      .getByRole('button', { name: 'Create application', exact: true })
      .click();
    await p
      .getByRole('button', { name: 'Check creation result', exact: true })
      .click();
    await p.getByText('Application found', { exact: true }).waitFor();
    await p.getByText(/Write attempts: 1/).waitFor();
    console.log('LOST RESPONSE recovery pass');
  } finally {
    await b.close();
  }
})().catch((e) => {
  console.error(e);
  process.exitCode = 1;
});
