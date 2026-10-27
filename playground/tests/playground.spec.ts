import { readFileSync } from 'node:fs';
import { test, expect, type Page } from '@playwright/test';
import type { EditorView } from '@codemirror/view';

// Replaces the source as one CodeMirror transaction. Input through the DOM
// selection races CodeMirror's focus handling, which can collapse the selection
// so the text is inserted at the old cursor instead.
async function source(page: Page, value: string) {
  const view = await page.waitForFunction(() =>
    (window as { playground?: { view: EditorView } }).playground?.view as EditorView);
  await view.evaluate((view, value) =>
    view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: value } }), value);
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
          const url = new URL(route.request().url());
          const path = url.pathname;
          if (path === '/comments') {
            const postId = Number(url.searchParams.get('postId'));
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
    for (const name of names.filter(name => name !== 'Blank')) {
      await page.locator('#example').selectOption(name);
      await expect(page.locator('#diagnostics-section')).toHaveAttribute('data-checked');
      await expect(page.locator('#diagnostics'), name).toBeEmpty();
      if (name === 'Hello, Do') continue;
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

test('editing during a run updates highlighting and diagnostics', async ({ page }) => {
  await source(page, 'while true\n  nil');
  await page.getByRole('button', { name: 'Run', exact: true }).click();
  await expect(page.locator('#status')).toHaveText('Running…');
  await source(page, 'echo 42\nmissing');
  await expect(page.locator('.token-number')).toHaveText('42');
  await expect(page.locator('#diagnostics')).not.toBeEmpty();
  await expect(page.locator('#status')).toHaveText('Running…');
  await page.getByRole('button', { name: 'Stop', exact: true }).click();
  await expect(page.locator('#status')).toHaveText('Ready');
});

test('replacing a stuck run worker keeps the analysis worker', async ({ page }) => {
  await page.addInitScript(() => {
    const Original = Worker;
    const urls: string[] = [];
    (window as unknown as { workerUrls: string[] }).workerUrls = urls;
    window.Worker = class extends Original {
      constructor(url: string | URL, options?: WorkerOptions) {
        super(url, options);
        urls.push(String(url));
      }
    };
  });
  await page.reload();
  await expect(page.getByRole('button', { name: 'Run', exact: true })).toBeEnabled();
  await source(page, 'while true\n  nil');
  await page.getByRole('button', { name: 'Run', exact: true }).click();
  await expect(page.locator('#status')).toHaveText('Running…');
  await page.getByRole('button', { name: 'Stop', exact: true }).click();
  await expect(page.locator('#status')).toHaveText('Ready');
  const urls = await page.evaluate(() => (window as unknown as { workerUrls: string[] }).workerUrls);
  expect(urls.filter(url => /analysis-worker/.test(url))).toHaveLength(1);
  expect(urls.filter(url => /run-worker/.test(url))).toHaveLength(2);
  await source(page, 'echo 7');
  await expect(page.locator('.token-number')).toHaveText('7');
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

test('term styling renders and markup is escaped', async ({ page }) => {
  await source(page, 'import term\nlet warning = term.text warning fg: :RED: bold: true\necho $warning plain\necho "<b>&</b>"');
  await run(page);
  await expect(page.locator('#error')).toBeEmpty();
  await expect(page.locator('#output')).toHaveText('warning plain\n<b>&</b>\n');
  const styled = page.locator('#output span.ansi-red-fg');
  await expect(styled).toHaveText('warning');
  await expect(styled).toHaveCSS('font-weight', '700');
  await expect(page.locator('#output b')).toHaveCount(0);
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
  await expect(page.locator('#diagnostics-section')).toHaveAttribute('data-checked');
  // The checker can't see the module the import handler provides
  await expect(page.locator('#diagnostics li')).toHaveText(['error · line 44: module `dynamic` not found']);
  await run(page);
  await expect(page.locator('#error')).toBeEmpty();
  await expect(page.locator('#output')).toHaveText('extensions passed\n');
});

test('type errors are underlined and listed', async ({ page }) => {
  await source(page, '# é 😀\nlet x @ Int = "a"\necho $x');
  await expect(page.locator('#diagnostics-section')).toHaveAttribute('data-checked');
  await expect(page.locator('#diagnostics')).toContainText('error · line 2:');
  await expect(page.locator('.cm-lintRange-error')).toHaveCount(1);
  await source(page, '# é 😀\nlet x @ Int = 1\necho $x');
  await expect(page.locator('#diagnostics-section')).toHaveAttribute('data-checked');
  await expect(page.locator('#diagnostics')).toBeEmpty();
  await expect(page.locator('.cm-lintRange-error')).toHaveCount(0);
});

test('type checking waits for edits to settle', async ({ page }) => {
  type Reply = { type: string; version: number; at: number };
  await page.addInitScript(() => {
    const Original = Worker;
    const replies: Reply[] = [];
    (window as unknown as { replies: Reply[] }).replies = replies;
    window.Worker = class extends Original {
      override set onmessage(handler: ((this: Worker, event: MessageEvent) => unknown) | null) {
        super.onmessage = event => {
          const { type, version } = event.data;
          if (type === 'analysis' || type === 'checked') replies.push({ type, version, at: performance.now() });
          handler?.call(this, event);
        };
      }
    };
  });
  await page.reload();
  await expect(page.locator('#diagnostics-section')).toHaveAttribute('data-checked');
  for (const value of ['"first"', '"second"', '"third"']) {
    // Long enough for each edit to be analyzed, shorter than the check delay
    await source(page, `let x @ Int = ${value}\necho $x`);
    await page.waitForTimeout(220);
  }
  await expect(page.locator('#diagnostics-section')).toHaveAttribute('data-checked');
  await expect(page.locator('#diagnostics')).toContainText('"third"');
  await expect(page.locator('#diagnostics')).not.toContainText('"first"');
  await expect(page.locator('#diagnostics')).not.toContainText('"second"');
  const replies = await page.evaluate(() => (window as unknown as { replies: Reply[] }).replies);
  const analyses = replies.filter(reply => reply.type === 'analysis');
  for (const [index, analysis] of analyses.entries()) {
    const check = replies.find(reply => reply.type === 'checked' && reply.version === analysis.version);
    // A check comes only once its source has been left alone for the delay...
    if (check) expect(check.at - analysis.at).toBeGreaterThanOrEqual(250);
    // ...so a source edited again well within it is never checked.
    const next = analyses[index + 1];
    if (next && next.at - analysis.at < 250) expect(check).toBeUndefined();
  }
  const checked = replies.filter(reply => reply.type === 'checked').length;
  expect(checked).toBeLessThan(analyses.length);
});

// Moves the mouse onto the character at `pos` and returns the hover tooltip.
async function hoverAt(page: Page, pos: number) {
  const view = await page.waitForFunction(() =>
    (window as { playground?: { view: EditorView } }).playground?.view as EditorView);
  const { x, y } = await view.evaluate((view, pos) => {
    const start = view.coordsAtPos(pos, 1)!;
    const end = view.coordsAtPos(pos + 1, -1)!;
    return { x: (start.left + end.right) / 2, y: (start.top + start.bottom) / 2 };
  }, pos);
  await page.mouse.move(0, 0);
  await page.mouse.move(x, y);
  return page.locator('.cm-tooltip-hover');
}

test('hover shows checked types before the check delay ends', async ({ page }) => {
  const text = 'let n = 2\necho $n';
  await source(page, text);
  const tip = await hoverAt(page, text.length - 1);
  await expect(tip).toHaveText('let n @ std.Int = ...');
});

test('hover shows a local function signature and doc comment', async ({ page }) => {
  const text = 'let n = 2\n# Doubles a *number*.\ndef double x@Int -> Int\n  (x * 2)\ndouble $n';
  await source(page, text);
  const tip = await hoverAt(page, text.lastIndexOf('double') + 1);
  await expect(tip.locator('pre')).toHaveText('def double x @ Int -> Int\n  ...');
  await expect(tip.locator('em')).toHaveText('number');
  await expect(tip).toContainText('Doubles a number.');
  // Its signature says what the checked type would
  await expect(tip).not.toContainText('std.Int');
});

test('hover shows a prelude function signature and doc', async ({ page }) => {
  await source(page, 'echo hi');
  const tip = await hoverAt(page, 1);
  await expect(tip.locator('pre').first()).toHaveText('# module term\ndef echo ...args @ Value\n  ...');
  await expect(tip).toContainText('Prints arguments separated by spaces');
  // Its signature says what the checked type would
  await expect(tip).not.toContainText('term.echo @');
  // Cross-references go to the API docs beside the playground, apart from it
  const text = tip.locator('a', { hasText: 'Text' }).first();
  await expect(text).toHaveAttribute('href', '../api/term/Text/#term.Text');
  await expect(text).toHaveAttribute('target', '_blank');
});

test('hover on an overloaded bundled call shows the chosen overload', async ({ page }) => {
  await source(page, 'echo (int "ff" base: 16)');
  const tip = await hoverAt(page, 7);
  await expect(tip.locator('pre')).toHaveText('# module std\ndef int value@Str :base@Int -> Int\n  ...');
});

test('hover shows a method of a bundled class', async ({ page }) => {
  const text = 'echo ("hi".upper())';
  await source(page, text);
  const tip = await hoverAt(page, text.indexOf('upper') + 1);
  await expect(tip.locator('pre').first()).toHaveText(/^# module std\s+class Str\s+def upper/);
  await expect(tip).toContainText('Returns the string converted to uppercase.');
});

test('hover escapes markup and refuses unsafe links in doc comments', async ({ page }) => {
  const text = 'let x = 1\n# A <b>value</b> <img src=x onerror="document.body.dataset.hacked = 1">.\nlet z = x\necho $z';
  await source(page, text);
  const tip = await hoverAt(page, text.indexOf('let z') + 4);
  await expect(tip).toContainText('A <b>value</b> <img src=x onerror="document.body.dataset.hacked = 1">.');
  await expect(tip.locator('b, img')).toHaveCount(0);
  await expect(page.locator('body')).not.toHaveAttribute('data-hacked');
  const linking = 'let x = 1\n# A [link](javascript:alert(1)).\nlet y = x\necho $y';
  await source(page, linking);
  const linked = await hoverAt(page, linking.length - 1);
  await expect(linked).toContainText('link');
  await expect(linked.locator('a[href^="javascript"]')).toHaveCount(0);
});

test('an edit drops a shown or pending hover', async ({ page }) => {
  await page.addInitScript(() => {
    const Original = Worker;
    window.Worker = class extends Original {
      override set onmessage(handler: ((this: Worker, event: MessageEvent) => unknown) | null) {
        super.onmessage = event => {
          if (event.data.type === 'hover' && document.documentElement.dataset.holdHover) {
            // Deliver the answer after the edit that replaces its text.
            setTimeout(() => handler?.call(this, event), 1000);
            document.documentElement.dataset.heldHover = 'yes';
          } else {
            handler?.call(this, event);
          }
        };
      }
    };
  });
  await page.reload();
  await expect(page.getByRole('button', { name: 'Run', exact: true })).toBeEnabled();
  const text = 'let n = 2\necho $n';
  await source(page, text);
  const tip = await hoverAt(page, text.length - 1);
  await expect(tip).toHaveText('let n @ std.Int = ...');
  await source(page, `${text}\n`);
  await expect(tip).toHaveCount(0);
  await page.evaluate(() => { document.documentElement.dataset.holdHover = 'yes'; });
  await hoverAt(page, text.length - 1);
  await expect(page.locator('html')).toHaveAttribute('data-held-hover', 'yes');
  await source(page, 'let s = "a"\necho $s');
  await page.waitForTimeout(1100);
  await expect(tip).toHaveCount(0);
});
