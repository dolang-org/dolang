// The little Markdown that diagnostics and doc summaries use inline: code
// quoted between backticks.

const codeSpan = /(`+)(.+?)\1/g;

/** `text` with its code spans as `code` elements */
export function markup(text: string): (Node | string)[] {
  const result: (Node | string)[] = [];
  let at = 0;
  for (const match of text.matchAll(codeSpan)) {
    const code = document.createElement('code');
    code.textContent = match[2];
    result.push(text.slice(at, match.index), code);
    at = match.index! + match[0].length;
  }
  result.push(text.slice(at));
  return result.filter(part => part !== '');
}

/** `text` as shown, without the backticks around its code spans */
export const plain = (text: string) => text.replace(codeSpan, '$2');
