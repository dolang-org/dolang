import { readFileSync } from 'node:fs';
import { test, expect, type Page } from '@playwright/test';

async function source(page: Page, value: string) {
  const editor = page.getByRole('textbox', { name: 'Do source' });
  await editor.fill(value);
}
async function run(page: Page) {
  await page.getByRole('button', { name: 'Run', exact: true }).click();
  await expect(page.locator('#status')).toHaveText(/Finished|Failed/);
}

for (const path of ['/', '/repo/playground/']) {
  test(`production assets and examples at ${path}`, async ({ page }) => {
    const errors: string[] = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.goto(path);
    await run(page);
    await expect(page.locator('#output')).toHaveText('Hello, world!\n');
    const names = await page.locator('#example option').evaluateAll(options =>
      options.map(option => (option as HTMLOptionElement).value));
    for (const name of names.filter(name => name !== 'Hello, Do')) {
      await page.locator('#example').selectOption(name);
      await run(page);
      await expect(page.locator('#error')).toBeEmpty();
      await expect(page.locator('#output')).not.toBeEmpty();
    }
    expect(errors).toEqual([]);
  });
}

test.beforeEach(async ({ page }) => {
  await page.goto('/');
  await expect(page.getByRole('button', { name: 'Run', exact: true })).toBeEnabled();
});

test('errors, captured output, and fresh VM state', async ({ page }) => {
  await source(page, 'echo before\nthrow std.RuntimeError "oops"');
  await run(page);
  await expect(page.locator('#output')).toHaveText('before\n');
  await expect(page.locator('#error')).toContainText('oops');
  await expect(page.locator('#error-section')).toBeVisible();
  await source(page, 'let x = 42\necho $x');
  await run(page);
  await expect(page.locator('#output')).toHaveText('42\n');
  await expect(page.locator('#error-section')).toBeHidden();
  await source(page, 'x');
  await run(page);
  await expect(page.locator('#diagnostics')).not.toBeEmpty();
  await expect(page.locator('#diagnostics-section')).toBeVisible();
  await source(page, 'echo (1.25 + 2.5)');
  await run(page);
  await expect(page.locator('#output')).toHaveText('3.75\n');
  await expect(page.locator('#diagnostics-section')).toBeHidden();
  await source(page, 'echo (1099511627776 + 1)');
  await run(page);
  await expect(page.locator('#output')).toHaveText('1099511627777\n');
});

test('Unicode highlighting survives incomplete source and rapid edits', async ({ page }) => {
  await source(page, 'let broken =');
  await source(page, 'echo "é é 😀"\nlet x = 42\nlet =');
  await expect(page.locator('.token-number')).toHaveText('42');
  await expect(page.locator('#diagnostics')).not.toBeEmpty();
  await source(page, '# final\necho "ok"');
  await expect(page.locator('.token-comment')).toHaveText('# final');
  await expect(page.locator('.token-number')).toHaveCount(0);
  await expect(page.locator('#diagnostics')).toBeEmpty();
});

test('Stop replaces a busy worker and runs the edited source', async ({ page }) => {
  await source(page, 'while true\n  nil');
  await page.getByRole('button', { name: 'Run', exact: true }).click();
  await expect(page.locator('#status')).toHaveText('Running…');
  await source(page, 'echo recovered\necho 42');
  await page.getByRole('button', { name: 'Stop', exact: true }).click();
  await expect(page.locator('#status')).toHaveText('Ready');
  await expect(page.locator('.token-number')).toHaveText('42');
  await run(page);
  await expect(page.locator('#output')).toHaveText('recovered\n42\n');
});

test('completed run keeps its source snapshot while latest analysis resumes', async ({ page }) => {
  await source(page, 'let x = 0\nwhile (x < 1000000)\n  x = (x + 1)\necho $x');
  await page.getByRole('button', { name: 'Run', exact: true }).click();
  await source(page, 'echo "new source"');
  await expect(page.locator('#status')).toHaveText('Finished');
  await expect(page.locator('#output')).toHaveText('1000000\n');
  await expect(page.locator('#snapshot')).toHaveText('(earlier source)');
  await expect(page.locator('.token-number')).toHaveCount(0);
  await expect(page.locator('#diagnostics')).toBeEmpty();
});

test('delayed obsolete analysis cannot replace current highlights', async ({ page }) => {
  await page.addInitScript(() => {
    const Original = Worker;
    window.Worker = class extends Original {
      override set onmessage(handler: ((this: Worker, event: MessageEvent) => unknown) | null) {
        super.onmessage = event => {
          if (event.data.type === 'analysis' && event.data.value.tokens.some((token: { kind: string }) => token.kind === 'number')) {
            // Deliver the old document after the new document's reply.
            setTimeout(() => handler?.call(this, event), 1000);
            document.documentElement.dataset.delayedAnalysis = 'yes';
          } else {
            handler?.call(this, event);
          }
        };
      }
    };
  });
  await page.reload();
  await expect(page.getByRole('button', { name: 'Run', exact: true })).toBeEnabled();
  await source(page, '42');
  await expect(page.locator('html')).toHaveAttribute('data-delayed-analysis', 'yes');
  await source(page, '# latest');
  await expect(page.locator('.token-comment')).toHaveText('# latest');
  await page.waitForTimeout(1100);
  await expect(page.locator('.token-number')).toHaveCount(0);
  await expect(page.locator('.token-comment')).toHaveText('# latest');
});

test('Wasm initialization failures are visible', async ({ page }) => {
  await page.route('**/*.wasm', route => route.abort());
  await page.reload();
  await expect(page.locator('#status')).toContainText('Could not load playground');
  await expect(page.locator('#error')).not.toBeEmpty();
  await expect(page.getByRole('button', { name: 'Run', exact: true })).toBeDisabled();
});

test('boxed floating point special values work on Wasm', async ({ page }) => {
  await source(page, 'echo [str(1.0 / 0.0), str(-1.0 / 0.0), str(0.0 / 0.0), (0.0 / 0.0 == 0.0 / 0.0)]');
  await run(page);
  await expect(page.locator('#error')).toBeEmpty();
  await expect(page.locator('#output')).toHaveText('["inf", "-inf", "NaN", false]\n');
});

test('portable extensions and dynamic modules execute in Wasm', async ({ page }) => {
  await source(page, readFileSync(new URL('./extensions.dol', import.meta.url), 'utf8'));
  await run(page);
  await expect(page.locator('#error')).toBeEmpty();
  await expect(page.locator('#output')).toHaveText('extensions passed\n');
});
