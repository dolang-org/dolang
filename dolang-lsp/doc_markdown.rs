/// Removes mkdocs attribute-list anchors from heading lines.
///
/// VS Code renders hover documentation as Markdown but does not understand
/// mkdocs' `{: #anchor }` extension, so leaving these suffixes in the static
/// index exposes them as literal text.
pub(crate) fn remove_manual_anchors(markdown: &str) -> String {
    markdown
        .split_inclusive('\n')
        .map(|line| {
            let (content, newline) = line
                .strip_suffix('\n')
                .map_or((line, ""), |content| (content, "\n"));
            let trimmed = content.trim_end();
            let Some(start) = trimmed.rfind("{:") else {
                return line.to_owned();
            };
            let attribute = &trimmed[start + 2..];
            let Some(attribute) = attribute.strip_suffix('}') else {
                return line.to_owned();
            };
            let attribute = attribute.trim();
            let Some(anchor) = attribute.strip_prefix('#') else {
                return line.to_owned();
            };
            if anchor.is_empty() || anchor.chars().any(char::is_whitespace) {
                return line.to_owned();
            }
            format!("{}{}", content[..start].trim_end(), newline)
        })
        .collect()
}

/// Removes playground markup from fenced code blocks.
///
/// The mkdocs site links a fence tagged `playground` to the playground and
/// hides its `#>` lines, which carry setup such as imports (see
/// `docs/hooks/playground_hook.py`). Hover documentation keeps the fence but
/// drops the tag and the hidden lines.
pub(crate) fn remove_playground_markup(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    // Indentation and marker of the open playground fence, if any.
    let mut fence: Option<(&str, &str)> = None;
    for line in markdown.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let newline = &line[content.len()..];
        let rest = content.trim_start();
        let indent = &content[..content.len() - rest.len()];
        if let Some((open_indent, marker)) = fence {
            let close = rest.trim_end();
            if close.len() >= marker.len() && close.bytes().all(|b| b == marker.as_bytes()[0]) {
                fence = None;
            } else if content
                .strip_prefix(open_indent)
                .is_some_and(|code| code.starts_with("#>"))
            {
                continue;
            }
            out.push_str(line);
            continue;
        }
        let marker_char = match rest.as_bytes().first() {
            Some(b'`') => '`',
            Some(b'~') => '~',
            _ => {
                out.push_str(line);
                continue;
            }
        };
        let info = rest.trim_start_matches(marker_char);
        let marker = &rest[..rest.len() - info.len()];
        let tagged = info.trim_start();
        match tagged.strip_prefix("playground") {
            Some(after)
                if marker.len() >= 3 && after.chars().next().is_none_or(char::is_whitespace) =>
            {
                out.push_str(indent);
                out.push_str(marker);
                out.push_str(after);
                out.push_str(newline);
                fence = Some((indent, marker));
            }
            _ => out.push_str(line),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_playground_tag_and_hidden_lines() {
        assert_eq!(
            remove_playground_markup(
                "Text\n\n```playground\n#> import base64:\n#>   - encode\nencode hi\n```\n",
            ),
            "Text\n\n```\nencode hi\n```\n",
        );
        assert_eq!(
            remove_playground_markup(
                "- item\n\n  ````playground title=x\n  #> hidden\n  shown\n  ````\n"
            ),
            "- item\n\n  ```` title=x\n  shown\n  ````\n",
        );
    }

    #[test]
    fn preserves_other_fences_and_hidden_prefix_outside_playground() {
        let markdown = "```\n#> kept\n```\n```playgrounds\n#> kept\n```\n#> kept\n";
        assert_eq!(remove_playground_markup(markdown), markdown);
    }

    #[test]
    fn removes_mkdocs_manual_anchor_suffixes() {
        assert_eq!(
            remove_manual_anchors(
                "## Busy retry {: #sqlite.Connection.busy-retry }\n\nText\n### Body {: #body}\n",
            ),
            "## Busy retry\n\nText\n### Body\n",
        );
    }

    #[test]
    fn preserves_other_attribute_lists_and_brace_text() {
        let markdown = "## Red {.warning}\nUse {: #anchor } in prose.\nIncomplete {: #anchor\n";
        assert_eq!(remove_manual_anchors(markdown), markdown);
    }
}
