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

#[cfg(test)]
mod tests {
    use super::*;

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
