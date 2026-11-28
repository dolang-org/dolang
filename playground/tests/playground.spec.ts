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
      await expect(page.locator('#problems')).toHaveAttribute('data-checked');
      await expect(page.locator('#problems'), name).toBeEmpty();
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
  await expect(page.locator('#problems')).not.toBeEmpty();
  await expect(page.locator('#problems-count')).not.toBeEmpty();
  await source(page, 'echo (1.25 + 2.5)');
  await run(page);
  await expect(page.locator('#output')).toHaveText('3.75\n');
  await expect(page.locator('#problems-count')).toBeEmpty();
  await source(page, 'echo (1099511627776 + 1)');
  await run(page);
  await expect(page.locator('#output')).toHaveText('1099511627777\n');
});

test('Unicode highlighting survives incomplete source and rapid edits', async ({ page }) => {
  await source(page, 'let broken =');
  await source(page, 'echo "é é 😀"\nlet x = 42\nlet =');
  await expect(page.locator('.token-number')).toHaveText('42');
  await expect(page.locator('#problems')).not.toBeEmpty();
  await source(page, '# final\necho "ok"');
  await expect(page.locator('.token-comment')).toHaveText('# final');
  await expect(page.locator('.token-number')).toHaveCount(0);
  await expect(page.locator('#problems')).toBeEmpty();
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
  await expect(page.locator('#backtrace .frame-head').last()).toHaveText(/<main>line 3$/);
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
  await expect(page.locator('#problems')).not.toBeEmpty();
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
  await expect(page.locator('#problems')).toBeEmpty();
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
  await expect(page.locator('#problems')).toHaveAttribute('data-checked');
  // The checker can't see the module the import handler provides
  await expect(page.locator('#problems .problem-head')).toHaveText([/^errormodule dynamic not foundline 44$/]);
  await run(page);
  await expect(page.locator('#error')).toBeEmpty();
  await expect(page.locator('#output')).toHaveText('extensions passed\n');
});

test('type errors are underlined and listed', async ({ page }) => {
  await source(page, '# é 😀\nlet x @ Int = "a"\necho $x');
  await expect(page.locator('#problems')).toHaveAttribute('data-checked');
  await expect(page.locator('#problems .problem.error .problem-line')).toHaveText('line 2');
  await expect(page.locator('.cm-lintRange-error')).toHaveCount(1);
  await source(page, '# é 😀\nlet x @ Int = 1\necho $x');
  await expect(page.locator('#problems')).toHaveAttribute('data-checked');
  await expect(page.locator('#problems')).toBeEmpty();
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
  await expect(page.locator('#problems')).toHaveAttribute('data-checked');
  for (const value of ['"first"', '"second"', '"third"']) {
    // Long enough for each edit to be analyzed, shorter than the check delay
    await source(page, `let x @ Int = ${value}\necho $x`);
    await page.waitForTimeout(220);
  }
  await expect(page.locator('#problems')).toHaveAttribute('data-checked');
  await expect(page.locator('#problems')).toContainText('"third"');
  await expect(page.locator('#problems')).not.toContainText('"first"');
  await expect(page.locator('#problems')).not.toContainText('"second"');
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
  await expect(tip).toHaveText('let n @ 2 = ...');
  // Its fence is highlighted as source is
  await expect(tip.locator('pre .token-keyword')).toHaveText('let');
});

test('hover shows a local function signature and doc comment', async ({ page }) => {
  const text = 'let n = 2\n# Doubles a *number*.\ndef double x@Int -> Int\n  (x * 2)\ndouble $n';
  await source(page, text);
  const tip = await hoverAt(page, text.lastIndexOf('double') + 1);
  await expect(tip.locator('pre')).toHaveText('def double x @ Int -> Int\n  ...');
  await expect(tip.locator('pre .token-keyword').first()).toHaveText('def');
  await expect(tip.locator('em')).toHaveText('number');
  await expect(tip).toContainText('Doubles a number.');
  // Its signature says what the checked type would
  await expect(tip).not.toContainText('std.Int');
});

test('hover shows a local alias where a type names it', async ({ page }) => {
  const text = 'let w = 0\n# Two of a kind.\npub @let Pair[T] = Tuple[T, T]\ndef first p@(Pair[Int])\n  p[0]';
  await source(page, text);
  const tip = await hoverAt(page, text.lastIndexOf('Pair') + 1);
  await expect(tip.locator('pre')).toHaveText('pub @let Pair[T] = Tuple[T, T]');
  await expect(tip).toContainText('Two of a kind.');
});

test('hover keeps the lines of a vertical alias', async ({ page }) => {
  const text = 'let w = 0\n@let Service = Dict $\n  "name": Str\n  ?"replicas": Int\ndef name service @ Service\n  service["name"]';
  await source(page, text);
  const tip = await hoverAt(page, text.lastIndexOf('Service') + 1);
  await expect(tip.locator('pre')).toHaveText('@let Service = Dict $\n  "name": Str\n  ?"replicas": Int');
  await expect(tip.locator('pre .token-keyword').first()).toHaveText('let');
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
  // The implementation's doc comment, which the overload has, holds examples
  await expect(tip.locator('pre', { hasText: '# module' }))
    .toHaveText('# module std\ndef int value@Str :base@Int -> Int\n  ...');
});

test('hover on an overloaded bundled name shows its overloads with the implementation doc', async ({ page }) => {
  const text = 'let f = int\necho $f';
  await source(page, text);
  // The hint after `f` moves the name once the check finishes
  await expect(page.locator('.cm-inlay-hint')).toHaveCount(1);
  const tip = await hoverAt(page, text.indexOf('int') + 1);
  await expect(tip.locator('pre', { hasText: '# module' })).toHaveText([
    '# module std\ndef int value @ (Bool | Int | Float | Str) -> Int\n  ...',
    '# module std\ndef int value@Str :base@Int -> Int\n  ...',
  ]);
  await expect(tip.getByText('Coerces or parses a value as an Int.')).toHaveCount(2);
});

test('hover shows a local function input and output', async ({ page }) => {
  const text = 'def parse_log x <Iter[Str] >Sink[(Str, Str)]\n  nil\nparse_log 1';
  await source(page, text);
  const tip = await hoverAt(page, text.lastIndexOf('parse_log') + 1);
  await expect(tip.locator('pre')).toHaveText('def parse_log x <Iter[Str] >Sink[(Str, Str)]\n  ...');
});

test('hover on an overloaded local function shows its overloads', async ({ page }) => {
  const text = 'let n = 1\n# Twice a number\n@def twice x @ Int -> Int\n@def twice x @ Str -> Str\n'
    + '# Twice anything\ndef twice x\n  (x + x)\nlet f = twice\necho $f';
  await source(page, text);
  await expect(page.locator('.cm-inlay-hint')).toHaveCount(1);
  const tip = await hoverAt(page, text.lastIndexOf('twice') + 1);
  await expect(tip.locator('pre')).toHaveText(['@def twice x @ Int -> Int', '@def twice x @ Str -> Str']);
  await expect(tip).toContainText('Twice a number');
  await expect(tip).toContainText('Twice anything');
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
  await expect(tip).toHaveText('let n @ 2 = ...');
  await source(page, `${text}\n`);
  await expect(tip).toHaveCount(0);
  await page.evaluate(() => { document.documentElement.dataset.holdHover = 'yes'; });
  await hoverAt(page, text.length - 1);
  await expect(page.locator('html')).toHaveAttribute('data-held-hover', 'yes');
  await source(page, 'let s = "a"\necho $s');
  await page.waitForTimeout(1100);
  await expect(tip).toHaveCount(0);
});

const editorText = (page: Page) => page.getByRole('textbox', { name: 'Do source' });
const savedDraft = (page: Page) => page.evaluate(() => localStorage.getItem('dolang-playground:draft'));

test('choosing an example asks before replacing edits', async ({ page }) => {
  const dialogs: string[] = [];
  page.on('dialog', dialog => {
    dialogs.push(dialog.message());
    return dialogs.length === 1 ? dialog.dismiss() : dialog.accept();
  });
  // Unedited text is replaced without asking
  await page.locator('#example').selectOption('Blank');
  await expect(editorText(page)).toHaveText('');
  expect(dialogs).toEqual([]);
  await source(page, 'echo edited');
  await page.locator('#example').selectOption('Hello, Do');
  await expect(page.locator('#example')).toHaveValue('Blank');
  await expect(editorText(page)).toHaveText('echo edited');
  await page.locator('#example').selectOption('Hello, Do');
  await expect(page.locator('#example')).toHaveValue('Hello, Do');
  await expect(editorText(page)).toContainText('Hello');
  expect(dialogs).toHaveLength(2);
});

test('a draft survives a reload, and a shared link wins over it without replacing it', async ({ page }) => {
  await source(page, 'echo shared');
  await page.getByRole('button', { name: 'Share link' }).click();
  await expect.poll(() => page.url()).toContain('?src=');
  const shared = page.url();
  await source(page, 'echo draft');
  await expect.poll(() => savedDraft(page)).toContain('echo draft');
  await page.goto('/');
  await expect(editorText(page)).toHaveText('echo draft');
  // A restored draft is an edit, so replacing it asks
  let asked = false;
  page.once('dialog', dialog => { asked = true; return dialog.dismiss(); });
  await page.locator('#example').selectOption('Blank');
  await expect(editorText(page)).toHaveText('echo draft');
  expect(asked).toBe(true);
  await page.goto(shared);
  await expect(editorText(page)).toHaveText('echo shared');
  await page.goto('/');
  await expect(editorText(page)).toHaveText('echo draft');
});

// Replaces the source and puts the cursor at its end, for typing.
async function sourceAtEnd(page: Page, value: string) {
  await source(page, value);
  await page.evaluate(() => {
    const { view } = (window as unknown as { playground: { view: EditorView } }).playground;
    view.dispatch({ selection: { anchor: view.state.doc.length } });
    view.focus();
  });
}
const doc = (page: Page) => page.evaluate(() =>
  (window as unknown as { playground: { view: EditorView } }).playground.view.state.doc.toString());

test('Enter indents by the VS Code extension rules', async ({ page }) => {
  const cases: [before: string, indent: string][] = [
    ['def f x', '  '],
    ['  if ready', '    '],
    ['else', '  '],
    ['# def comment', ''],
    ['let y = if a', '  '],
    ['let f = do |x|', '  '],
    ['let doc = |', '  '],
    ['let items = $', '  '],
    ['import math:', '  '],
    ['echo plain', ''],
    ['  echo plain', '  '],
    ['  return 1', ''],
  ];
  for (const [before, indent] of cases) {
    await sourceAtEnd(page, before);
    await page.keyboard.press('Enter');
    expect(await doc(page), before).toBe(`${before}\n${indent}`);
  }
});

test('catch, finally and else dedent to their headers, and Backspace removes a level', async ({ page }) => {
  await sourceAtEnd(page, 'try\n  if x\n    risky()');
  await page.keyboard.press('Enter');
  await page.keyboard.type('catch');
  expect(await doc(page)).toBe('try\n  if x\n    risky()\ncatch');
  await sourceAtEnd(page, 'let r = try\n  risky()');
  await page.keyboard.press('Enter');
  await page.keyboard.type('finally');
  expect(await doc(page)).toBe('let r = try\n  risky()\nfinally');
  const typed: [before: string, after: string][] = [
    ['if a\n  if b\n    x', '  else'],
    ['let v = if a\n  x', 'else'],
    ['if a\n  x\nelse if b\n  y', 'else'],
    // One with no `if` belongs to a `let`, so stays put
    ['def f\n  let k v = pair', '  else'],
  ];
  for (const [before, after] of typed) {
    await sourceAtEnd(page, before);
    await page.keyboard.press('Enter');
    await page.keyboard.type('else');
    expect(await doc(page), before).toBe(`${before}\n${after}`);
  }
  await sourceAtEnd(page, 'def f\n  if x\n    ');
  await page.keyboard.press('Backspace');
  expect(await doc(page)).toBe('def f\n  if x\n  ');
});

test('editor search, bracket closing and selection matches', async ({ page }) => {
  await sourceAtEnd(page, 'let total = 1\necho $total');
  await page.keyboard.type('(');
  expect(await doc(page)).toBe('let total = 1\necho $total()');
  await page.keyboard.press('ControlOrMeta+f');
  await expect(page.locator('.cm-search')).toBeVisible();
  await page.keyboard.press('Escape');
  await page.evaluate(() => {
    const { view } = (window as unknown as { playground: { view: EditorView } }).playground;
    view.dispatch({ selection: { anchor: 4, head: 9 } });
  });
  await expect(page.locator('.cm-selectionMatch')).toHaveCount(1);
});

test('the cursor marks the names of its declaration', async ({ page }) => {
  await sourceAtEnd(page, 'let total = 1\necho $total\necho other');
  await page.evaluate(() => {
    const { view } = (window as unknown as { playground: { view: EditorView } }).playground;
    view.dispatch({ selection: { anchor: 6 } });
  });
  await expect(page.locator('.cm-occurrence')).toHaveText(['total', 'total']);
  // The name it binds is a write
  await expect(page.locator('.cm-occurrence-write')).toHaveText(['total']);
  // A word naming no declaration marks none
  await page.evaluate(() => {
    const { view } = (window as unknown as { playground: { view: EditorView } }).playground;
    view.dispatch({ selection: { anchor: view.state.doc.length - 2 } });
  });
  await expect(page.locator('.cm-occurrence')).toHaveCount(0);
});

test('type hints follow checks, and clicking one inserts its annotation', async ({ page }) => {
  await source(page, 'let a = (1 + 2)\nlet x = 1\necho $a $x');
  await expect(page.locator('#problems')).toHaveAttribute('data-checked');
  // A literal binding gets none
  const hint = page.locator('.cm-inlay-hint');
  await expect(hint).toHaveText([' @ Int']);
  await expect(hint).toHaveAttribute('title', 'Int\nClick to insert');
  // An edit drops them until the next check
  await source(page, 'let b = (1 + 2)\necho $b');
  await expect(hint).toHaveCount(0);
  await expect(page.locator('#problems')).toHaveAttribute('data-checked');
  await hint.click();
  expect(await doc(page)).toBe('let b @ Int = (1 + 2)\necho $b');
  await expect(page.locator('#problems')).toHaveAttribute('data-checked');
  await expect(hint).toHaveCount(0);
});

test('the type hints toggle hides them and is kept across reloads', async ({ page }) => {
  const toggle = page.getByLabel('Type hints');
  await expect(toggle).toBeChecked();
  await source(page, 'let a = (1 + 2)\necho $a');
  const hint = page.locator('.cm-inlay-hint');
  await expect(hint).toHaveCount(1);
  await toggle.uncheck();
  await expect(hint).toHaveCount(0);
  await expect.poll(() => savedDraft(page)).toContain('let a');
  await page.reload();
  await expect(page.locator('#problems')).toHaveAttribute('data-checked');
  await expect(toggle).not.toBeChecked();
  await expect(hint).toHaveCount(0);
  // Shown again without another check
  await toggle.check();
  await expect(hint).toHaveText([' @ Int']);
});

test('a long type hint is cut short and titled in full', async ({ page }) => {
  await source(page, 'let long = [{a: 1, b: [(1 + 2)], c: :x:, d: nil, e: [(1 + 2.0)]}]\necho $long');
  const hint = page.locator('.cm-inlay-hint');
  await expect(hint).toHaveText(/^ @ Array\[Dict\[.*…$/);
  const label = (await hint.textContent())!;
  const title = (await hint.getAttribute('title'))!;
  expect(title.startsWith(label.slice(3, -1))).toBe(true);
  expect(title.split('\n')[0].length).toBeGreaterThan(label.length - 3);
});

test('a summary says how each run ended', async ({ page }) => {
  await source(page, 'echo hi');
  await run(page);
  await expect(page.locator('#run-summary')).toHaveText(/^Finished in (\d+ ms|\d+\.\d\d s)$/);
  await source(page, 'throw std.RuntimeError "oops"');
  await run(page);
  await expect(page.locator('#run-summary')).toHaveText(/^Raised an error after /);
  await source(page, 'import time\ntime.sleep 1000');
  await page.getByRole('button', { name: 'Run', exact: true }).click();
  await expect(page.locator('#run-summary')).toBeEmpty();
  await expect(page.locator('#status')).toHaveText('Running…');
  await page.getByRole('button', { name: 'Stop', exact: true }).click();
  await expect(page.locator('#run-summary')).toHaveText(/^Cancelled after /);
});

test('output can be copied and cleared', async ({ page, context }) => {
  await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  await source(page, 'echo first\nthrow std.RuntimeError "oops"');
  await run(page);
  await page.getByRole('button', { name: 'Copy' }).click();
  await expect(page.getByRole('button', { name: 'Copied!' })).toBeVisible();
  const copied = await page.evaluate(() => navigator.clipboard.readText());
  expect(copied).toMatch(/^first\n.*oops/s);
  await page.getByRole('button', { name: 'Clear' }).click();
  await expect(page.locator('#output')).toBeEmpty();
  await expect(page.locator('#run-summary')).toBeEmpty();
  await expect(page.locator('#error-section')).toBeHidden();
});

test('the divider resizes the editor and is kept across reloads', async ({ page }) => {
  const editorWidth = () => page.locator('section[aria-label="Source"]').evaluate(element => element.getBoundingClientRect().width);
  const divider = page.getByRole('separator', { name: 'Resize editor and results' });
  await expect(divider).toHaveAttribute('aria-valuenow', '60');
  const before = await editorWidth();
  await divider.focus();
  await page.keyboard.press('ArrowLeft');
  await page.keyboard.press('ArrowLeft');
  await expect(divider).toHaveAttribute('aria-valuenow', '50');
  expect(await editorWidth()).toBeLessThan(before);
  // Dragging follows the pointer, within limits
  const box = (await page.locator('#workspace').boundingBox())!;
  const handle = (await divider.boundingBox())!;
  await page.mouse.move(handle.x + 2, handle.y + 20);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width * 0.05, handle.y + 20);
  await page.mouse.up();
  await expect(divider).toHaveAttribute('aria-valuenow', '20');
  await page.reload();
  await expect(divider).toHaveAttribute('aria-valuenow', '20');
});

test('the results can be hidden, and a run shows them again', async ({ page }) => {
  await page.getByRole('button', { name: 'Hide results' }).click();
  await expect(page.locator('#execution')).toBeHidden();
  await expect(page.locator('#divider')).toBeHidden();
  await page.reload();
  await expect(page.locator('#execution')).toBeHidden();
  await expect(page.getByRole('button', { name: 'Run', exact: true })).toBeEnabled();
  await run(page);
  await expect(page.locator('#execution')).toBeVisible();
  await expect(page.getByRole('button', { name: 'Hide results' })).toBeVisible();
});

test('hover fits bundled examples and signatures without scrolling sideways', async ({ page }) => {
  const text = 'import digest\nimport base64\necho $digest.sha256("abc")\necho $base64.encode("abc")';
  await source(page, text);
  for (const name of ['sha256', 'encode']) {
    const tip = await hoverAt(page, text.lastIndexOf(name) + 1);
    await expect(tip.locator('pre').first()).toBeVisible();
    const overflowing = await tip.locator('pre').evaluateAll(pres =>
      pres.filter(pre => pre.scrollWidth > pre.clientWidth).map(pre => pre.textContent));
    expect(overflowing, name).toEqual([]);
  }
});

test('the results have Output and Problems tabs, and a run shows Output', async ({ page }) => {
  await source(page, 'x');
  const problemsTab = page.getByRole('tab', { name: /Problems/ });
  await expect(problemsTab).toHaveText('Problems1');
  await problemsTab.click();
  await expect(problemsTab).toHaveAttribute('aria-selected', 'true');
  await expect(page.locator('#problems-panel')).toBeVisible();
  await expect(page.locator('#output-panel')).toBeHidden();
  await expect(page.getByRole('button', { name: 'Copy' })).toBeHidden();
  await problemsTab.press('ArrowLeft');
  await expect(page.getByRole('tab', { name: 'Output' })).toBeFocused();
  await expect(page.locator('#output-panel')).toBeVisible();
  await problemsTab.click();
  await run(page);
  await expect(page.getByRole('tab', { name: 'Output' })).toHaveAttribute('aria-selected', 'true');
});

test('a problem card shows its lines, labels and fixes', async ({ page }) => {
  await source(page, 'let name = "é"\necho name $name');
  await page.getByRole('tab', { name: /Problems/ }).click();
  const card = page.locator('.problem.lint');
  await expect(card.locator('.problem-head')).toContainText('bareword is the literal string "name"');
  await expect(card.locator('.snippet-number')).toHaveText(['1', '', '2', '']);
  await expect(card.locator('.snippet-text')).toHaveText([
    'let name = "é"',
    '    ──── variable bound here',
    'echo name $name',
    '     ━━━━',
  ], { useInnerText: true });
  await card.hover();
  await expect(page.locator('.cm-problem-span')).toHaveText(['name', 'name']);
  await card.getByRole('button', { name: 'Fix: write $ for the variable' }).click();
  await expect.poll(() => doc(page)).toBe('let name = "é"\necho $name $name');
  await expect(page.locator('#problems')).toHaveAttribute('data-checked');
  await expect(page.locator('.problem.lint')).toHaveCount(0);
});

test('problems are marked in the gutter, and F8 steps through them', async ({ page }) => {
  const text = 'let name = 1\necho name $name\necho name';
  await source(page, text);
  await expect(page.locator('.cm-lint-marker-info')).toHaveCount(2);
  await page.getByRole('textbox', { name: 'Do source' }).click();
  await page.evaluate(() => {
    const { view } = (window as unknown as { playground: { view: EditorView } }).playground;
    view.dispatch({ selection: { anchor: 0 } });
  });
  const selection = () => page.evaluate(() => {
    const { main } = (window as unknown as { playground: { view: EditorView } }).playground.view.state.selection;
    return [main.from, main.to];
  });
  const first = text.indexOf('name', 13);
  const second = text.lastIndexOf('name');
  await page.keyboard.press('F8');
  await expect.poll(selection).toEqual([first, first + 4]);
  await page.keyboard.press('F8');
  await expect.poll(selection).toEqual([second, second + 4]);
  await page.keyboard.press('Shift+F8');
  await expect.poll(selection).toEqual([first, first + 4]);
  await expect(page.getByRole('tab', { name: /Problems/ })).toHaveAttribute('aria-selected', 'false');
});

test('problems of an earlier source are dimmed until analyzed', async ({ page }) => {
  await source(page, 'x');
  await expect(page.locator('#problems')).toHaveAttribute('data-checked');
  const stale = await page.evaluate(() => {
    const { view } = (window as unknown as { playground: { view: EditorView } }).playground;
    view.dispatch({ changes: { from: 1, insert: 'y' } });
    return document.getElementById('problems')!.classList.contains('stale');
  });
  expect(stale).toBe(true);
  await expect(page.locator('#problems')).toHaveAttribute('data-checked');
  await expect(page.locator('#problems')).not.toHaveClass(/stale/);
});

test('a label in a bundled module links to its docs', async ({ page }) => {
  await source(page, 'class C: @std.BaseIter[Int]\n  pub def (next) _self -> Str => "a"\necho (C())');
  await page.getByRole('tab', { name: /Problems/ }).click();
  const note = page.locator('.problem.error .problem-note.elsewhere');
  await expect(note).toHaveText('in std: as BaseIter.(next) declares it');
  await expect(note.getByRole('link', { name: 'std' })).toHaveAttribute('href', /\/api\/std\/$/);
  await expect(note.locator('code')).toHaveText('BaseIter.(next)');
  await expect(page.locator('.problem.error .problem-message code')).toHaveText(['(next)', 'BaseIter.(next)']);
  // A message too long to fit beside its span hangs below it
  await expect(page.locator('.problem.error .snippet-text')).toHaveText([
    '  pub def (next) _self -> Str => "a"',
    '           ━━━━           ───',
    '                          │',
    '                          ╰╴return type: std.Str here, but std.Int in BaseIter.(next)',
  ], { useInnerText: true });
});

test('Help lists the shortcuts and the bundled modules, linked and summarized', async ({ page }) => {
  await page.getByRole('button', { name: 'Help' }).click();
  const dialog = page.getByRole('dialog', { name: 'Using the playground' });
  await expect(dialog).toBeVisible();
  await expect(dialog.locator('.shortcuts dt').first()).toHaveText(/^(Ctrl|⌘)\+Enter$/);
  const modules = dialog.locator('.modules li');
  await expect(modules).toHaveCount(20);
  await expect(dialog.getByRole('link', { name: 'base64' })).toHaveAttribute('href', '../api/base64/');
  await expect(modules.filter({ hasText: 'base64' }).locator('.module-summary'))
    .toHaveText('Base64 encoding and decoding (RFC 4648).');
  await expect(modules.filter({ hasText: 'test' }).locator('.module-summary')).toContainText('Only its assertions are available here.');
  await page.keyboard.press('Escape');
  await expect(dialog).toBeHidden();
});

test('an error shows its backtrace, linked to the source as it changes', async ({ page, context }) => {
  await context.grantPermissions(['clipboard-read', 'clipboard-write']);
  await source(page, 'import json\n\ndef load text\n  json.decode $text\n\nload "{"\n');
  await run(page);
  await expect(page.locator('#error')).toHaveText(/^EOF while parsing/);
  await expect(page.locator('#backtrace .frame-head')).toHaveText(['0json.decode', '1loadline 4', '2<main>line 6']);
  await expect(page.locator('#backtrace .snippet-number')).toHaveText(['4', '', '6', '']);
  await expect(page.locator('#backtrace .snippet-text')).toHaveText(['  json.decode $text', '  ━━━━━━━━━━━━━━━━━', 'load "{"', '━━━━━━━━']);
  // The code that raised is marked until it changes
  await expect(page.locator('.cm-raised')).toHaveText('json.decode $text');
  await expect(page.locator('.cm-raised-line')).toHaveCount(1);
  await page.getByRole('button', { name: 'Copy' }).click();
  const copied = await page.evaluate(() => navigator.clipboard.readText());
  expect(copied).toMatch(/EOF while parsing[^\n]*\n {2}0: json\.decode\n {2}1: load at line 4\n {2}2: <main> at line 6\n$/);
  await page.evaluate(() => {
    const { view } = (window as unknown as { playground: { view: EditorView } }).playground;
    view.dispatch({ changes: { from: 0, insert: '# moved\n' } });
  });
  await expect(page.locator('.cm-raised')).toHaveCount(0);
  await page.getByRole('button', { name: 'line 4' }).click();
  const selected = await page.evaluate(() => {
    const { state } = (window as unknown as { playground: { view: EditorView } }).playground.view;
    return state.sliceDoc(state.selection.main.from, state.selection.main.to);
  });
  expect(selected).toBe('json.decode $text');
});

test('Revert restores the loaded example after asking', async ({ page }) => {
  const revert = page.getByRole('button', { name: 'Revert' });
  await page.locator('#example').selectOption('Hello, Do');
  await expect(revert).toBeDisabled();
  const original = await doc(page);
  await source(page, 'echo edited');
  await expect(revert).toBeEnabled();
  page.once('dialog', dialog => dialog.dismiss());
  await revert.click();
  expect(await doc(page)).toBe('echo edited');
  page.once('dialog', dialog => dialog.accept());
  await revert.click();
  await expect(revert).toBeDisabled();
  expect(await doc(page)).toBe(original);
  // Undo brings the edits back
  await expect(page.locator('.cm-content')).toBeFocused();
  await page.keyboard.press('ControlOrMeta+z');
  await expect.poll(() => doc(page)).toBe('echo edited');
  // Blank has nothing to revert to
  page.once('dialog', dialog => dialog.accept());
  await page.locator('#example').selectOption('Blank');
  await source(page, 'echo edited');
  await expect(revert).toBeDisabled();
});
