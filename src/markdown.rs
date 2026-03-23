pub fn markdown_to_telegram_html(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut blocks = Vec::new();
    let lines: Vec<&str> = normalized.lines().collect();
    let mut i = 0usize;

    while i < lines.len() {
        let trimmed = lines[i].trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        if let Some(fence) = fence_delimiter(trimmed) {
            i += 1;
            let mut code_lines = Vec::new();
            while i < lines.len() {
                let current = lines[i].trim();
                if current.starts_with(fence) {
                    i += 1;
                    break;
                }
                code_lines.push(lines[i]);
                i += 1;
            }
            blocks.push(format!(
                "<pre><code>{}</code></pre>",
                escape_html(&code_lines.join("\n"))
            ));
            continue;
        }

        if let Some(content) = heading_text(trimmed) {
            blocks.push(format!("<b>{}</b>", render_inline(content.trim())));
            i += 1;
            continue;
        }

        if trimmed.starts_with('>') {
            let mut quote_lines = Vec::new();
            while i < lines.len() {
                let current = lines[i].trim();
                if current.is_empty() || !current.starts_with('>') {
                    break;
                }
                let text = current.trim_start_matches('>').trim_start();
                quote_lines.push(render_inline(text));
                i += 1;
            }
            blocks.push(format!(
                "<blockquote>{}</blockquote>",
                quote_lines.join("\n")
            ));
            continue;
        }

        if let Some(item) = unordered_list_item(trimmed) {
            let mut items = vec![format!("• {}", render_inline(item.trim()))];
            i += 1;
            while i < lines.len() {
                let current = lines[i].trim();
                if let Some(next) = unordered_list_item(current) {
                    items.push(format!("• {}", render_inline(next.trim())));
                    i += 1;
                } else {
                    break;
                }
            }
            blocks.push(items.join("\n"));
            continue;
        }

        if let Some(item) = ordered_list_item(trimmed) {
            let mut items = vec![format!("1. {}", render_inline(item.trim()))];
            let mut n = 2usize;
            i += 1;
            while i < lines.len() {
                let current = lines[i].trim();
                if let Some(next) = ordered_list_item(current) {
                    items.push(format!("{}. {}", n, render_inline(next.trim())));
                    n += 1;
                    i += 1;
                } else {
                    break;
                }
            }
            blocks.push(items.join("\n"));
            continue;
        }

        let mut paragraph = vec![trimmed];
        i += 1;
        while i < lines.len() {
            let current = lines[i].trim();
            if current.is_empty()
                || fence_delimiter(current).is_some()
                || heading_text(current).is_some()
                || current.starts_with('>')
                || unordered_list_item(current).is_some()
                || ordered_list_item(current).is_some()
            {
                break;
            }
            paragraph.push(current);
            i += 1;
        }
        blocks.push(render_inline(&paragraph.join("\n")));
    }

    blocks.join("\n\n")
}

pub fn sanitize_telegram_html(text: &str) -> String {
    text.replace("<name>", "&lt;name&gt;")
        .replace("</name>", "&lt;/name&gt;")
        .replace("<thinking>", "&lt;thinking&gt;")
        .replace("</thinking>", "&lt;/thinking&gt;")
}

pub fn split_telegram_message(text: &str, limit: usize) -> Vec<String> {
    if text.chars().count() <= limit {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();

    for line in text.lines() {
        let next_len = current.chars().count() + line.chars().count() + 1;
        if !current.is_empty() && next_len > limit {
            chunks.push(current.trim_end().to_string());
            current.clear();
        }
        current.push_str(line);
        current.push('\n');
    }

    if !current.trim().is_empty() {
        chunks.push(current.trim_end().to_string());
    }

    chunks
}

fn render_inline(text: &str) -> String {
    let (protected, code_spans) = protect_code_spans(text);
    let mut result = render_links(&escape_html(&protected));

    while let Some(start) = result.find("**") {
        if let Some(end_rel) = result[start + 2..].find("**") {
            let end = start + 2 + end_rel;
            let inner = result[start + 2..end].to_string();
            result = format!("{}<b>{}</b>{}", &result[..start], inner, &result[end + 2..]);
        } else {
            break;
        }
    }

    let chars: Vec<char> = result.chars().collect();
    let mut output = String::new();
    let mut i = 0usize;
    let mut italic = false;
    while i < chars.len() {
        if chars[i] == '*'
            && (i == 0 || chars[i - 1] != '*')
            && (i + 1 >= chars.len() || chars[i + 1] != '*')
        {
            if italic {
                output.push_str("</i>");
            } else {
                output.push_str("<i>");
            }
            italic = !italic;
        } else {
            output.push(chars[i]);
        }
        i += 1;
    }
    restore_code_spans(output, &code_spans)
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn protect_code_spans(text: &str) -> (String, Vec<String>) {
    let mut output = String::new();
    let mut code_spans = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;

    while i < chars.len() {
        if chars[i] == '`'
            && let Some(end_rel) = chars[i + 1..].iter().position(|c| *c == '`')
        {
            let end = i + 1 + end_rel;
            let inner: String = chars[i + 1..end].iter().collect();
            let placeholder = format!("@@CODE_SPAN_{}@@", code_spans.len());
            code_spans.push(format!("<code>{}</code>", escape_html(&inner)));
            output.push_str(&placeholder);
            i = end + 1;
            continue;
        }
        output.push(chars[i]);
        i += 1;
    }

    (output, code_spans)
}

fn restore_code_spans(mut text: String, code_spans: &[String]) -> String {
    for (index, html) in code_spans.iter().enumerate() {
        let placeholder = format!("@@CODE_SPAN_{index}@@");
        text = text.replace(&placeholder, html);
    }
    text
}

fn render_links(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut output = String::new();
    let mut i = 0usize;

    while i < chars.len() {
        if chars[i] == '['
            && let Some((label, href, next_index)) = parse_markdown_link(&chars, i)
        {
            output.push_str("<a href=\"");
            output.push_str(&escape_html_attribute(&href));
            output.push_str("\">");
            output.push_str(&label);
            output.push_str("</a>");
            i = next_index;
            continue;
        }

        output.push(chars[i]);
        i += 1;
    }

    output
}

fn parse_markdown_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let label_end_rel = chars[start + 1..].iter().position(|c| *c == ']')?;
    let label_end = start + 1 + label_end_rel;
    if chars.get(label_end + 1) != Some(&'(') {
        return None;
    }

    let mut depth = 0usize;
    let mut url_end = None;
    let mut i = label_end + 2;
    while i < chars.len() {
        match chars[i] {
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    url_end = Some(i);
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
        i += 1;
    }

    let url_end = url_end?;
    let label: String = chars[start + 1..label_end].iter().collect();
    let raw_target: String = chars[label_end + 2..url_end].iter().collect();
    let href = parse_link_target(&raw_target)?;
    Some((label, href, url_end + 1))
}

fn parse_link_target(target: &str) -> Option<String> {
    let trimmed = target.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(rest) = trimmed.strip_prefix('<') {
        let end = rest.find('>')?;
        let candidate = rest[..end].trim();
        if candidate.is_empty() {
            return None;
        }
        return Some(candidate.to_string());
    }

    let href = trimmed.split_whitespace().next()?;
    if href.is_empty() {
        return None;
    }
    Some(href.to_string())
}

fn escape_html_attribute(text: &str) -> String {
    text.replace('"', "&quot;")
}

fn fence_delimiter(line: &str) -> Option<&'static str> {
    if line.starts_with("```") {
        Some("```")
    } else if line.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn heading_text(line: &str) -> Option<&str> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && line.chars().nth(hashes) == Some(' ') {
        Some(&line[hashes + 1..])
    } else {
        None
    }
}

fn unordered_list_item(line: &str) -> Option<&str> {
    for prefix in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(prefix) {
            return Some(rest);
        }
    }
    None
}

fn ordered_list_item(line: &str) -> Option<&str> {
    let digit_count = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digit_count == 0 {
        return None;
    }
    let rest = &line[digit_count..];
    rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_basic_markdown() {
        let rendered = markdown_to_telegram_html("## Title\n\n**bold** and `code`");
        assert!(rendered.contains("<b>Title</b>"));
        assert!(rendered.contains("<b>bold</b>"));
        assert!(rendered.contains("<code>code</code>"));
    }

    #[test]
    fn preserves_markdown_link_literal_inside_code_span() {
        let rendered = markdown_to_telegram_html("`[x](url \"title\")`");
        assert_eq!(rendered, "<code>[x](url \"title\")</code>");
    }

    #[test]
    fn renders_links_with_nested_parentheses_and_brackets_in_target() {
        let rendered = markdown_to_telegram_html(
            "[file](/Users/test/app/[lang]/(app)/(no-header)/component.tsx#L32)",
        );
        assert_eq!(
            rendered,
            "<a href=\"/Users/test/app/[lang]/(app)/(no-header)/component.tsx#L32\">file</a>"
        );
        assert_eq!(rendered.matches("<a ").count(), 1);
    }

    #[test]
    fn strips_optional_markdown_link_title_and_escapes_quotes_in_href() {
        let rendered = markdown_to_telegram_html(
            "[title](https://example.com/path?q=\"x\" \"Example title\")",
        );
        assert_eq!(
            rendered,
            "<a href=\"https://example.com/path?q=&quot;x&quot;\">title</a>"
        );
    }

    #[test]
    fn splits_large_messages() {
        let text = format!("{}\n{}", "a".repeat(3000), "b".repeat(3000));
        let chunks = split_telegram_message(&text, 4096);
        assert_eq!(chunks.len(), 2);
    }
}
