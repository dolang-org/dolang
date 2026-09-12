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
    // The HTTP example calls a public API; answer it locally to keep tests offline.
    let routed = 0;
    await page.context().route('https://jsonplaceholder.typicode.com/**', route => {
      routed++;
      const cors = { 'Access-Control-Allow-Origin': '*', 'Access-Control-Allow-Headers': '*' };
      switch (route.request().method()) {
        case 'OPTIONS':
          return route.fulfill({ status: 204, headers: { ...cors, 'Access-Control-Allow-Methods': 'GET, POST' } });
        case 'POST':
          return route.fulfill({ status: 201, headers: cors, json: { ...route.request().postDataJSON(), id: 101 } });
        default: {
          const path = new URL(route.request().url()).pathname;
          const comments = /^\/posts\/(\d+)\/comments$/.exec(path);
          if (comments) {
            const postId = Number(comments[1]);
            return route.fulfill({
              headers: cors,
              json: [1, 2, 3].map(n => ({ postId, id: postId * 10 + n, name: `comment ${n}`, email: 'do@example.com', body: 'first line\nsecond line' })),
            });
          }
          if (path === '/posts') {
            return route.fulfill({
              headers: cors,
              json: Array.from({ length: 10 }, (_, i) => ({ userId: 1, id: i + 1, title: `post ${i + 1}`, body: 'post body' })),
            });
          }
          return route.fulfill({ status: 404, headers: cors, json: {} });
        }
      }
    });
    const names = await page.locator('#example option').evaluateAll(options =>
      options.map(option => (option as HTMLOptionElement).value));
    for (const name of names.filter(name => name !== 'Hello, Do' && name !== 'Blank')) {
      await page.locator('#example').selectOption(name);
      await run(page);
      await expect(page.locator('#error')).toBeEmpty();
      await expect(page.locator('#output')).not.toBeEmpty();
    }
    expect(routed).toBeGreaterThan(0);
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

test('echo output streams live before a run finishes', async ({ page }) => {
  await source(page, 'echo first\nwhile true\n  nil');
  await page.getByRole('button', { name: 'Run', exact: true }).click();
  await expect(page.locator('#status')).toHaveText('Running…');
  await expect(page.locator('#output')).toHaveText('first\n');
  await page.getByRole('button', { name: 'Stop', exact: true }).click();
  await expect(page.locator('#status')).toHaveText('Ready');
});

test('Stop cancels a suspended run and keeps its output', async ({ page }) => {
  await source(page, 'import time\necho start\ntry\n  time.sleep 1000\nfinally\n  echo cleanup');
  await page.getByRole('button', { name: 'Run', exact: true }).click();
  await expect(page.locator('#output')).toHaveText('start\n');
  await page.getByRole('button', { name: 'Stop', exact: true }).click();
  await expect(page.locator('#status')).toHaveText('Stopped');
  await expect(page.locator('#output')).toHaveText('start\ncleanup\n');
  await expect(page.locator('#error')).toContainText('canceled');
  await expect(page.locator('#error')).toContainText('at ');
  await expect(page.getByRole('button', { name: 'Run', exact: true })).toBeEnabled();
  await source(page, 'echo again');
  await run(page);
  await expect(page.locator('#output')).toHaveText('again\n');
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

test('Share link round-trips compressed source and selects Blank on load', async ({ page }) => {
  await source(page, 'echo shared-source');
  await page.getByRole('button', { name: 'Share link' }).click();
  await expect.poll(() => page.url()).toContain('?src=');
  const url = page.url();
  await page.goto(url);
  await expect(page.getByRole('button', { name: 'Run', exact: true })).toBeEnabled();
  await expect(page.getByRole('textbox', { name: 'Do source' })).toHaveText('echo shared-source');
  await expect(page.locator('#example')).toHaveValue('Blank');
  await run(page);
  await expect(page.locator('#output')).toHaveText('shared-source\n');
});

test('http requests run through fetch in Wasm', async ({ page, baseURL }) => {
  await source(page, `let base = "${baseURL}"\n` + readFileSync(new URL('./http.dol', import.meta.url), 'utf8'));
  await run(page);
  await expect(page.locator('#error')).toBeEmpty();
  await expect(page.locator('#output')).toHaveText('http passed\n');
});

test('portable extensions and dynamic modules execute in Wasm', async ({ page }) => {
  await source(page, readFileSync(new URL('./extensions.dol', import.meta.url), 'utf8'));
  await run(page);
  await expect(page.locator('#error')).toBeEmpty();
  await expect(page.locator('#output')).toHaveText('extensions passed\n');
});
