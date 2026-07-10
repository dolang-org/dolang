#![deny(warnings)]

use std::{
    fs,
    io::{self, Read},
    ops::ControlFlow,
    path::{Path, PathBuf},
};

use clap::Parser;
use dolang::compile::{Compiler, Context, Origin, Token};
use serde_json::{Value, json};

#[derive(Parser)]
struct Cli {
    /// Module name for the output (used as identifier prefix)
    #[arg(long)]
    module: Option<String>,
    /// Include non-pub entities (default: pub only)
    #[arg(long)]
    all: bool,
    /// Source path (reads stdin if omitted)
    path: Option<PathBuf>,
}

/// A collected token with its span information
struct TokEntry {
    token: Token,
    /// byte offset of start
    start: usize,
    /// 0-based line of start
    line: u32,
    /// 0-based column of start
    col: u32,
    /// byte offset of end
    end: usize,
    origin: Option<Origin>,
}

fn collect_tokens(path: &Path, content: &[u8]) -> Vec<TokEntry> {
    let compiler = Compiler::new(path, content);
    let mut entries: Vec<TokEntry> = Vec::new();
    let _ = compiler.analyze(
        &mut |_diag| ControlFlow::Continue(()),
        &mut |token, span: dolang::compile::Span, origin, _ctx: Context| -> ControlFlow<()> {
            entries.push(TokEntry {
                token,
                start: span.start().byte_offset(),
                line: span.start().line_offset(),
                col: span.start().column_offset(),
                end: span.end().byte_offset(),
                origin,
            });
            ControlFlow::Continue(())
        },
    );
    // Sort by start offset; tokens are not guaranteed to be in source order
    entries.sort_by_key(|e| e.start);
    entries
}

/// Extract the text slice from source bytes for a given byte range
fn src_text(content: &[u8], start: usize, end: usize) -> &str {
    std::str::from_utf8(&content[start..end]).unwrap_or("")
}

/// Determine if a token is a definition site (token span matches origin def span)
fn is_def_site(entry: &TokEntry) -> bool {
    match &entry.origin {
        Some(Origin::Class { span }) => span.start().byte_offset() == entry.start,
        Some(Origin::Def { span })
        | Some(Origin::Bind { span })
        | Some(Origin::Method { span, .. })
        | Some(Origin::Field { span, .. }) => span.start().byte_offset() == entry.start,
        Some(Origin::Param { span }) => span.start().byte_offset() == entry.start,
        Some(Origin::SelfParam { span }) => span.start().byte_offset() == entry.start,
        _ => false,
    }
}

/// Scan backward from index `i` to find the preceding block of contiguous comment lines.
/// Returns the comment text with `# ` stripped, joined as markdown (in forward order).
fn extract_doc(entries: &[TokEntry], i: usize, content: &[u8]) -> String {
    // The def name is on some line L. Find the start line by scanning back through
    // keywords on the same line (pub/def/class/let may precede the name).
    let def_line = entries[i].line;

    // Scan backward collecting comment tokens on lines def_line-1, def_line-2, etc.
    // Stop as soon as there's a gap.
    let mut comment_lines: Vec<String> = Vec::new();
    let mut expected_line = def_line.saturating_sub(1);
    let mut j = i;
    loop {
        if j == 0 {
            break;
        }
        j -= 1;
        let e = &entries[j];
        match &e.token {
            Token::Comment => {
                if e.line == expected_line {
                    let raw = src_text(content, e.start, e.end);
                    // Strip leading `# ` or `#`
                    let stripped = raw
                        .strip_prefix("# ")
                        .or_else(|| raw.strip_prefix('#'))
                        .unwrap_or(raw);
                    comment_lines.push(stripped.to_owned());
                    if expected_line > 0 {
                        expected_line -= 1;
                    } else {
                        break;
                    }
                } else if e.line < expected_line {
                    // Gap — stop
                    break;
                }
                // e.line > expected_line: comment on same def line or after — skip
            }
            // Non-comment tokens on preceding lines break the contiguous block
            _ if e.line < expected_line => break,
            _ => {}
        }
    }
    comment_lines.reverse();
    comment_lines.join("\n")
}

/// Determine if a token is a special method definition (e.g. `(init)`).
/// Special methods are emitted as `Token::Keyword`; the `(` precedes the span start.
fn is_special_method(entry: &TokEntry, content: &[u8]) -> bool {
    matches!(entry.token, Token::Method) && entry.start > 0 && content[entry.start - 1] == b'('
}

/// Determine if the entity at index `i` is `pub` by counting keyword tokens on the same line.
/// Two keywords on the same line before the name → pub (e.g. `pub def` or `pub class`).
fn is_pub(entries: &[TokEntry], i: usize) -> bool {
    let def_line = entries[i].line;
    let def_start = entries[i].start;
    let mut keyword_count = 0;
    // Scan backward on the same line
    let mut j = i;
    loop {
        if j == 0 {
            break;
        }
        j -= 1;
        let e = &entries[j];
        if e.line != def_line {
            break;
        }
        if e.start >= def_start {
            continue;
        }
        if matches!(e.token, Token::Keyword) {
            keyword_count += 1;
        }
    }
    keyword_count >= 2
}

/// Collect parameters following a def name at index `i`.
/// Returns `(name, optional)` pairs. Skips `self` and `SelfParam`.
/// Stops on the first Keyword token (signals entry into the function body).
/// A param is optional when the next non-param token before the following param
/// (or end of params) is `Token::Delim` with text `=`.
fn collect_params(entries: &[TokEntry], i: usize, content: &[u8]) -> Vec<(String, bool)> {
    // First pass: collect indices of all param tokens
    let mut param_indices: Vec<usize> = Vec::new();
    for (j, e) in entries[(i + 1)..].iter().enumerate() {
        if matches!(e.token, Token::Keyword) {
            break;
        }
        if matches!(e.token, Token::Variable)
            && is_def_site(e)
            && matches!(
                e.origin,
                Some(Origin::Param { .. }) | Some(Origin::SelfParam { .. })
            )
        {
            param_indices.push(j + i + 1);
        }
    }

    // Second pass: for each param, determine if it has a default (`=` before next param)
    let mut params: Vec<(String, bool)> = Vec::new();
    for (k, &pi) in param_indices.iter().enumerate() {
        let e = &entries[pi];
        // Skip self parameters
        if matches!(e.origin, Some(Origin::SelfParam { .. })) {
            continue;
        }
        // Detect keyword params by scanning back for Token::Key.
        // For `:foo` style, Key and Variable overlap (Key is at pi-1 in sorted order).
        // For `foo: local` style, Key is at pi-2 with a Delim `:` at pi-1.
        // In both cases scan back over at most a Delim, then check for Key.
        let key_name: Option<String> = 'key: {
            let mut j = pi;
            loop {
                if j == 0 {
                    break 'key None;
                }
                j -= 1;
                match entries[j].token {
                    Token::Key => {
                        let kn = src_text(content, entries[j].start, entries[j].end);
                        break 'key Some(format!(":{kn}"));
                    }
                    Token::Delim => {} // skip the `:` separator
                    _ => break 'key None,
                }
            }
        };
        // Detect rest params: Token::Sigil immediately before the param variable
        let is_rest = key_name.is_none() && pi > 0 && matches!(entries[pi - 1].token, Token::Sigil);
        let name = if let Some(kn) = key_name {
            kn
        } else if is_rest {
            format!("...{}", src_text(content, e.start, e.end))
        } else {
            src_text(content, e.start, e.end).to_owned()
        };

        // Scan forward from pi+1 up to the next param index (or end) for `=`
        let next_param_pos = param_indices.get(k + 1).copied().unwrap_or(entries.len());
        let mut optional = false;
        for te in entries[(pi + 1)..next_param_pos.min(entries.len())].iter() {
            if matches!(te.token, Token::Keyword) {
                break;
            }
            if matches!(te.token, Token::Delim) && src_text(content, te.start, te.end) == "=" {
                optional = true;
                break;
            }
        }
        params.push((name, optional));
    }
    params
}

/// Collect superclass references for a class definition at index `i`.
/// Scans forward on the same line for Variable tokens after the class name.
fn collect_supers(entries: &[TokEntry], i: usize, content: &[u8]) -> Vec<Value> {
    let class_line = entries[i].line;
    let class_end = entries[i].end;
    let mut supers = Vec::new();
    for e in entries[i + 1..].iter() {
        if e.line != class_line {
            break;
        }
        if e.start < class_end {
            continue;
        }
        if matches!(e.token, Token::Variable) {
            let name = src_text(content, e.start, e.end);
            let super_ref = match &e.origin {
                Some(Origin::ImportItem { module, item, .. }) => {
                    let mod_name = src_text(
                        content,
                        module.start().byte_offset(),
                        module.end().byte_offset(),
                    );
                    let item_name = src_text(
                        content,
                        item.start().byte_offset(),
                        item.end().byte_offset(),
                    );
                    json!({ "module": mod_name, "item": item_name })
                }
                Some(Origin::ImportModule { module, .. }) => {
                    let mod_name = src_text(
                        content,
                        module.start().byte_offset(),
                        module.end().byte_offset(),
                    );
                    json!({ "module": mod_name })
                }
                _ => json!(name),
            };
            supers.push(super_ref);
        }
    }
    supers
}

fn params_json(params: Vec<(String, bool)>) -> Value {
    Value::Array(
        params
            .into_iter()
            .map(|(name, optional)| json!({"name": name, "optional": optional}))
            .collect(),
    )
}

fn span_json(entry: &TokEntry) -> Value {
    json!({
        "line": entry.line,
        "col": entry.col,
        "offset": entry.start,
    })
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let (path, content) = if let Some(path) = &cli.path {
        (path.as_ref(), fs::read(path)?)
    } else {
        let mut content = vec![];
        io::stdin().read_to_end(&mut content)?;
        (Path::new("<stdin>"), content)
    };

    let entries = collect_tokens(path, &content);

    // Build a map from class name byte offset → list of member JSON objects,
    // so we can group members under their class.
    let mut class_members: std::collections::HashMap<usize, Vec<Value>> =
        std::collections::HashMap::new();
    // Track class info keyed by class ident byte offset
    let mut class_info: std::collections::HashMap<usize, Value> = std::collections::HashMap::new();
    // Top-level entities (non-class-members)
    let mut top_level: Vec<(usize, Value)> = Vec::new();

    let n = entries.len();
    for i in 0..n {
        let e = &entries[i];
        let special = is_special_method(e, &content);
        if !(matches!(e.token, Token::Variable | Token::Method | Token::Field) || special)
            || !is_def_site(e)
        {
            continue;
        }
        let pub_flag = is_pub(&entries, i);
        if !pub_flag && !special && !cli.all {
            continue;
        }
        // For special methods, extend the name to include surrounding parens: (init)
        let name = if special {
            let end = if e.end < content.len() && content[e.end] == b')' {
                e.end + 1
            } else {
                e.end
            };
            src_text(&content, e.start - 1, end).to_owned()
        } else {
            src_text(&content, e.start, e.end).to_owned()
        };
        let doc = extract_doc(&entries, i, &content);
        let span = span_json(e);

        match &e.origin {
            Some(Origin::Class { .. }) => {
                let supers = collect_supers(&entries, i, &content);
                let obj = json!({
                    "kind": "class",
                    "name": name,
                    "pub": pub_flag,
                    "span": span,
                    "doc": doc,
                    "supers": supers,
                    "members": [],
                });
                class_info.insert(e.start, obj);
                top_level.push((e.start, Value::Null)); // placeholder, filled later
            }
            Some(Origin::Method { class: cls, .. }) => {
                let params = params_json(collect_params(&entries, i, &content));
                let member = json!({
                    "kind": "method",
                    "name": name,
                    "pub": pub_flag,
                    "special": special,
                    "span": span,
                    "doc": doc,
                    "params": params,
                });
                class_members
                    .entry(cls.start().byte_offset())
                    .or_default()
                    .push(member);
            }
            Some(Origin::Field { class: cls, .. }) => {
                let member = json!({
                    "kind": "field",
                    "name": name,
                    "pub": pub_flag,
                    "span": span,
                    "doc": doc,
                });
                class_members
                    .entry(cls.start().byte_offset())
                    .or_default()
                    .push(member);
            }
            Some(Origin::Def { .. }) => {
                let params = params_json(collect_params(&entries, i, &content));
                let obj = json!({
                    "kind": "function",
                    "name": name,
                    "pub": pub_flag,
                    "span": span,
                    "doc": doc,
                    "params": params,
                });
                top_level.push((e.start, obj));
            }
            Some(Origin::Bind { .. }) => {
                let obj = json!({
                    "kind": "value",
                    "name": name,
                    "pub": pub_flag,
                    "span": span,
                    "doc": doc,
                });
                top_level.push((e.start, obj));
            }
            _ => {}
        }
    }

    // Merge members into class objects and build final entity list
    let mut entities: Vec<Value> = Vec::new();
    for (offset, placeholder) in top_level {
        if placeholder.is_null() {
            // This is a class placeholder
            if let Some(mut class_obj) = class_info.remove(&offset) {
                let members = class_members.remove(&offset).unwrap_or_default();
                *class_obj.get_mut("members").unwrap() = Value::Array(members);
                entities.push(class_obj);
            }
        } else {
            entities.push(placeholder);
        }
    }

    let output = json!({
        "source": path.to_string_lossy(),
        "module": cli.module,
        "entities": entities,
    });

    println!("{}", output);
    Ok(())
}
