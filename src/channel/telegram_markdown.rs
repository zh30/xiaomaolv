use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use super::TelegramRenderedText;

#[derive(Debug, Clone, Copy)]
struct MarkdownListState {
    ordered: bool,
    next_index: usize,
}

pub(super) fn render_telegram_text_parts(raw: &str, max_chars: usize) -> Vec<TelegramRenderedText> {
    let (think, body) = split_think_and_body(raw);
    let source = if think.trim().is_empty() {
        raw.trim()
    } else {
        body.trim()
    };
    if source.is_empty() {
        return Vec::new();
    }

    let source_chunk_max = (max_chars.max(1) / 2).max(512);
    let mut parts = Vec::new();
    for chunk in split_text_chunks(source, source_chunk_max) {
        let rendered = render_markdown_to_telegram_markdown_v2(&chunk);
        if rendered.trim().is_empty() {
            continue;
        }
        if rendered.chars().count() <= max_chars.max(1) {
            parts.push(TelegramRenderedText {
                text: rendered,
                parse_mode: Some("MarkdownV2"),
            });
            continue;
        }

        for split in split_text_chunks(&rendered, max_chars.max(1)) {
            if split.trim().is_empty() {
                continue;
            }
            parts.push(TelegramRenderedText {
                text: split,
                parse_mode: Some("MarkdownV2"),
            });
        }
    }

    parts
}

pub(super) fn render_markdown_to_telegram_markdown_v2(input: &str) -> String {
    if input.trim().is_empty() {
        return String::new();
    }

    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);

    let parser = Parser::new_ext(input, options);
    let mut out = String::new();
    let mut line_start = true;
    let mut quote_depth = 0usize;
    let mut in_code_block = false;
    let mut list_stack: Vec<MarkdownListState> = Vec::new();
    let mut link_stack: Vec<String> = Vec::new();

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { .. } => {
                    ensure_blank_line(&mut out, &mut line_start);
                    out.push('*');
                    line_start = false;
                }
                Tag::Strong => {
                    out.push('*');
                    line_start = false;
                }
                Tag::Emphasis => {
                    out.push('_');
                    line_start = false;
                }
                Tag::Strikethrough => {
                    out.push('~');
                    line_start = false;
                }
                Tag::CodeBlock(kind) => {
                    ensure_blank_line(&mut out, &mut line_start);
                    in_code_block = true;
                    out.push_str("```");
                    if let CodeBlockKind::Fenced(lang) = kind {
                        let language = sanitize_code_fence_language(lang.as_ref());
                        if !language.is_empty() {
                            out.push_str(&language);
                        }
                    }
                    out.push('\n');
                    line_start = true;
                }
                Tag::List(start) => {
                    ensure_line_break(&mut out, &mut line_start);
                    list_stack.push(MarkdownListState {
                        ordered: start.is_some(),
                        next_index: start.unwrap_or(1) as usize,
                    });
                }
                Tag::Item => {
                    ensure_line_break(&mut out, &mut line_start);
                    if let Some(top) = list_stack.last_mut() {
                        if top.ordered {
                            out.push_str(&format!("{}\\. ", top.next_index));
                            top.next_index += 1;
                        } else {
                            out.push_str("• ");
                        }
                    } else {
                        out.push_str("• ");
                    }
                    line_start = false;
                }
                Tag::Link { dest_url, .. } => {
                    out.push('[');
                    line_start = false;
                    link_stack.push(dest_url.to_string());
                }
                Tag::BlockQuote(_) => {
                    ensure_line_break(&mut out, &mut line_start);
                    quote_depth += 1;
                }
                _ => {}
            },
            Event::End(tag_end) => match tag_end {
                TagEnd::Paragraph => ensure_blank_line(&mut out, &mut line_start),
                TagEnd::Heading(..) => {
                    out.push('*');
                    line_start = false;
                    ensure_blank_line(&mut out, &mut line_start);
                }
                TagEnd::Strong => {
                    out.push('*');
                    line_start = false;
                }
                TagEnd::Emphasis => {
                    out.push('_');
                    line_start = false;
                }
                TagEnd::Strikethrough => {
                    out.push('~');
                    line_start = false;
                }
                TagEnd::CodeBlock => {
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                    out.push_str("```");
                    line_start = false;
                    in_code_block = false;
                    ensure_blank_line(&mut out, &mut line_start);
                }
                TagEnd::List(_) => {
                    list_stack.pop();
                    ensure_blank_line(&mut out, &mut line_start);
                }
                TagEnd::Item => ensure_line_break(&mut out, &mut line_start),
                TagEnd::Link => {
                    let dest = link_stack.pop().unwrap_or_default();
                    out.push_str("](");
                    out.push_str(&escape_markdown_v2_link_destination(&dest));
                    out.push(')');
                    line_start = false;
                }
                TagEnd::BlockQuote(_) => {
                    quote_depth = quote_depth.saturating_sub(1);
                    ensure_line_break(&mut out, &mut line_start);
                }
                _ => {}
            },
            Event::Text(text) => {
                if in_code_block {
                    append_code_text(&mut out, text.as_ref(), &mut line_start, quote_depth, true);
                } else {
                    append_markdown_v2_text(&mut out, text.as_ref(), &mut line_start, quote_depth);
                }
            }
            Event::Code(text) => {
                maybe_push_quote_prefix(&mut out, &mut line_start, quote_depth);
                out.push('`');
                out.push_str(&escape_markdown_v2_code(text.as_ref()));
                out.push('`');
                line_start = false;
            }
            Event::SoftBreak | Event::HardBreak => {
                out.push('\n');
                line_start = true;
            }
            Event::Rule => {
                ensure_blank_line(&mut out, &mut line_start);
                append_markdown_v2_text(&mut out, "────────", &mut line_start, quote_depth);
                ensure_blank_line(&mut out, &mut line_start);
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "☑ " } else { "☐ " };
                append_markdown_v2_text(&mut out, marker, &mut line_start, quote_depth);
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                append_markdown_v2_text(&mut out, html.as_ref(), &mut line_start, quote_depth);
            }
            Event::FootnoteReference(name) => {
                append_markdown_v2_text(
                    &mut out,
                    format!("[{}]", name).as_str(),
                    &mut line_start,
                    quote_depth,
                );
            }
            _ => {}
        }
    }

    out.trim().to_string()
}

pub(super) fn sanitize_code_fence_language(raw: &str) -> String {
    raw.chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '+' | '.'))
        .collect()
}

pub(super) fn ensure_line_break(out: &mut String, line_start: &mut bool) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    *line_start = true;
}

pub(super) fn ensure_blank_line(out: &mut String, line_start: &mut bool) {
    if out.is_empty() {
        *line_start = true;
        return;
    }
    if out.ends_with("\n\n") {
        *line_start = true;
        return;
    }
    if out.ends_with('\n') {
        out.push('\n');
    } else {
        out.push_str("\n\n");
    }
    *line_start = true;
}

pub(super) fn maybe_push_quote_prefix(out: &mut String, line_start: &mut bool, quote_depth: usize) {
    if *line_start && quote_depth > 0 {
        for _ in 0..quote_depth {
            out.push_str("> ");
        }
        *line_start = false;
    }
}

pub(super) fn append_code_text(
    out: &mut String,
    input: &str,
    line_start: &mut bool,
    quote_depth: usize,
    in_code_block: bool,
) {
    for ch in input.chars() {
        if ch == '\n' {
            out.push('\n');
            *line_start = true;
            continue;
        }
        maybe_push_quote_prefix(out, line_start, quote_depth);
        if in_code_block {
            match ch {
                '\\' => out.push_str("\\\\"),
                '`' => out.push_str("\\`"),
                _ => out.push(ch),
            }
        } else {
            out.push(ch);
        }
        *line_start = false;
    }
}

pub(super) fn append_markdown_v2_text(
    out: &mut String,
    input: &str,
    line_start: &mut bool,
    quote_depth: usize,
) {
    for ch in input.chars() {
        if ch == '\n' {
            out.push('\n');
            *line_start = true;
            continue;
        }
        maybe_push_quote_prefix(out, line_start, quote_depth);
        if needs_markdown_v2_escape(ch) {
            out.push('\\');
        }
        out.push(ch);
        *line_start = false;
    }
}

pub(super) fn needs_markdown_v2_escape(ch: char) -> bool {
    matches!(
        ch,
        '\\' | '_'
            | '*'
            | '['
            | ']'
            | '('
            | ')'
            | '~'
            | '`'
            | '>'
            | '#'
            | '+'
            | '-'
            | '='
            | '|'
            | '{'
            | '}'
            | '.'
            | '!'
    )
}

pub(super) fn escape_markdown_v2_code(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '`' => out.push_str("\\`"),
            _ => out.push(ch),
        }
    }
    out
}

pub(super) fn escape_markdown_v2_link_destination(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            ')' => out.push_str("\\)"),
            _ => out.push(ch),
        }
    }
    out
}

pub(super) fn split_text_chunks(input: &str, max_chars: usize) -> Vec<String> {
    if input.is_empty() {
        return Vec::new();
    }
    let max_chars = max_chars.max(1);

    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut count = 0usize;

    for (idx, _) in input.char_indices() {
        if count == max_chars {
            chunks.push(input[start..idx].to_string());
            start = idx;
            count = 0;
        }
        count += 1;
    }

    if start < input.len() {
        chunks.push(input[start..].to_string());
    }

    chunks
}

pub(super) fn split_think_and_body(raw: &str) -> (String, String) {
    let mut remaining = raw;
    let mut think_parts: Vec<String> = Vec::new();
    let mut body = String::new();
    let open = "<think>";
    let close = "</think>";

    while let Some(start) = remaining.find(open) {
        body.push_str(&remaining[..start]);
        remaining = &remaining[start + open.len()..];

        if let Some(end) = remaining.find(close) {
            think_parts.push(remaining[..end].to_string());
            remaining = &remaining[end + close.len()..];
        } else {
            think_parts.push(remaining.to_string());
            remaining = "";
            break;
        }
    }

    body.push_str(remaining);
    (think_parts.join("\n\n"), body)
}
