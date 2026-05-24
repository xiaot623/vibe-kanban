use std::fmt::Write;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use teloxide::{
    RequestError,
    payloads::{EditMessageTextSetters, SendMessageSetters},
    prelude::{Bot, ChatId},
    requests::Requester,
    types::{InlineKeyboardMarkup, Message, MessageId, ParseMode, ThreadId},
};

pub(crate) const TELEGRAM_MESSAGE_LIMIT: usize = 3072;

#[derive(Debug, Clone, Copy)]
enum ListKind {
    Ordered(u64),
    Unordered,
}

pub(crate) fn escape_telegram_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

pub(crate) fn markdown_to_telegram_html(markdown: &str) -> String {
    let parser = Parser::new_ext(markdown, Options::ENABLE_STRIKETHROUGH);
    let mut out = String::with_capacity(markdown.len());
    let mut list_stack: Vec<ListKind> = Vec::new();

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { .. } => {
                    append_block_break(&mut out);
                    out.push_str("<b>");
                }
                Tag::BlockQuote(_) => {
                    append_block_break(&mut out);
                    out.push_str("&gt; ");
                }
                Tag::CodeBlock(_) => {
                    append_block_break(&mut out);
                    out.push_str("<pre><code>");
                }
                Tag::List(start) => {
                    list_stack.push(match start {
                        Some(index) => ListKind::Ordered(index),
                        None => ListKind::Unordered,
                    });
                    append_block_break(&mut out);
                }
                Tag::Item => {
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                    match list_stack.last_mut() {
                        Some(ListKind::Ordered(next)) => {
                            let _ = write!(out, "{}. ", *next);
                            *next = next.saturating_add(1);
                        }
                        _ => out.push_str("- "),
                    }
                }
                Tag::Emphasis => out.push_str("<i>"),
                Tag::Strong => out.push_str("<b>"),
                Tag::Strikethrough => out.push_str("<s>"),
                Tag::Link { dest_url, .. } => {
                    out.push_str("<a href=\"");
                    out.push_str(&escape_telegram_html(dest_url.as_ref()));
                    out.push_str("\">");
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => out.push('\n'),
                TagEnd::Heading(_) => {
                    out.push_str("</b>\n");
                }
                TagEnd::BlockQuote(_) => out.push('\n'),
                TagEnd::CodeBlock => {
                    out.push_str("</code></pre>\n");
                }
                TagEnd::List(_) => out.push('\n'),
                TagEnd::Item => {}
                TagEnd::Emphasis => out.push_str("</i>"),
                TagEnd::Strong => out.push_str("</b>"),
                TagEnd::Strikethrough => out.push_str("</s>"),
                TagEnd::Link => out.push_str("</a>"),
                _ => {}
            },
            Event::Text(text) => out.push_str(&escape_telegram_html(text.as_ref())),
            Event::Code(text) => {
                out.push_str("<code>");
                out.push_str(&escape_telegram_html(text.as_ref()));
                out.push_str("</code>");
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                out.push_str(&escape_telegram_html(text.as_ref()));
            }
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            Event::Rule => {
                append_block_break(&mut out);
                out.push_str("----\n");
            }
            Event::TaskListMarker(checked) => {
                out.push_str(if checked { "[x] " } else { "[ ] " });
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                out.push_str(&escape_telegram_html(text.as_ref()));
            }
            Event::FootnoteReference(name) => {
                let _ = write!(out, "[{}]", escape_telegram_html(name.as_ref()));
            }
        }
    }

    out.trim().to_string()
}

pub(crate) fn markdown_to_plain_text(markdown: &str) -> String {
    let parser = Parser::new_ext(markdown, Options::ENABLE_STRIKETHROUGH);
    let mut out = String::with_capacity(markdown.len());
    let mut list_stack: Vec<ListKind> = Vec::new();
    let mut link_stack: Vec<String> = Vec::new();

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Heading { .. } => append_block_break(&mut out),
                Tag::BlockQuote(_) => {
                    append_block_break(&mut out);
                    out.push_str("> ");
                }
                Tag::CodeBlock(_) => append_block_break(&mut out),
                Tag::List(start) => {
                    list_stack.push(match start {
                        Some(index) => ListKind::Ordered(index),
                        None => ListKind::Unordered,
                    });
                    append_block_break(&mut out);
                }
                Tag::Item => {
                    if !out.is_empty() && !out.ends_with('\n') {
                        out.push('\n');
                    }
                    match list_stack.last_mut() {
                        Some(ListKind::Ordered(next)) => {
                            let _ = write!(out, "{}. ", *next);
                            *next = next.saturating_add(1);
                        }
                        _ => out.push_str("- "),
                    }
                }
                Tag::Link { dest_url, .. } => link_stack.push(dest_url.to_string()),
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => out.push('\n'),
                TagEnd::Heading(_) => out.push('\n'),
                TagEnd::BlockQuote(_) => out.push('\n'),
                TagEnd::CodeBlock => out.push('\n'),
                TagEnd::List(_) => out.push('\n'),
                TagEnd::Link => {
                    if let Some(dest) = link_stack.pop()
                        && !dest.is_empty()
                    {
                        let _ = write!(out, " ({dest})");
                    }
                }
                _ => {}
            },
            Event::Text(text)
            | Event::Code(text)
            | Event::Html(text)
            | Event::InlineHtml(text)
            | Event::InlineMath(text)
            | Event::DisplayMath(text) => {
                out.push_str(text.as_ref());
            }
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            Event::Rule => {
                append_block_break(&mut out);
                out.push_str("----\n");
            }
            Event::TaskListMarker(checked) => {
                out.push_str(if checked { "[x] " } else { "[ ] " });
            }
            Event::FootnoteReference(name) => {
                let _ = write!(out, "[{name}]");
            }
        }
    }

    out.trim().to_string()
}

pub(crate) fn split_telegram_chunks(text: &str, limit: usize, reserve: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }

    let max_chars = limit.saturating_sub(reserve).max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;

    for ch in text.chars() {
        if current_len >= max_chars {
            chunks.push(current);
            current = String::new();
            current_len = 0;
        }

        current.push(ch);
        current_len += 1;
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    chunks
}

pub(crate) async fn send_rich_then_plain(
    bot: &Bot,
    chat_id: ChatId,
    markdown: &str,
    markup: Option<InlineKeyboardMarkup>,
) -> Result<Message, RequestError> {
    send_rich_then_plain_to_thread(bot, chat_id, None, markdown, markup).await
}

pub(crate) async fn send_rich_then_plain_to_thread(
    bot: &Bot,
    chat_id: ChatId,
    thread_id: Option<ThreadId>,
    markdown: &str,
    markup: Option<InlineKeyboardMarkup>,
) -> Result<Message, RequestError> {
    let payload = TelegramPayload::from_markdown(markdown);
    send_payload(bot, chat_id, thread_id, payload, markup).await
}

pub(crate) async fn edit_or_send_rich_then_plain(
    bot: &Bot,
    chat_id: ChatId,
    source_message_id: Option<MessageId>,
    markdown: &str,
    markup: Option<InlineKeyboardMarkup>,
) -> Result<Message, RequestError> {
    edit_or_send_rich_then_plain_to_thread(bot, chat_id, None, source_message_id, markdown, markup)
        .await
}

pub(crate) async fn edit_or_send_rich_then_plain_to_thread(
    bot: &Bot,
    chat_id: ChatId,
    thread_id: Option<ThreadId>,
    source_message_id: Option<MessageId>,
    markdown: &str,
    markup: Option<InlineKeyboardMarkup>,
) -> Result<Message, RequestError> {
    let payload = TelegramPayload::from_markdown(markdown);

    let Some(source_message_id) = source_message_id else {
        return send_payload(bot, chat_id, thread_id, payload, markup).await;
    };

    if let Some(rich) = payload.rich.as_ref() {
        let mut request = bot
            .edit_message_text(chat_id, source_message_id, rich.clone())
            .parse_mode(ParseMode::Html);
        if let Some(existing_markup) = markup.clone() {
            request = request.reply_markup(existing_markup);
        }

        match request.await {
            Ok(message) => return Ok(message),
            Err(err) => {
                tracing::debug!(
                    "Failed to edit Telegram message {} with rich HTML: {}",
                    source_message_id.0,
                    err
                );
            }
        }
    }

    let mut plain_request =
        bot.edit_message_text(chat_id, source_message_id, payload.plain.clone());
    if let Some(existing_markup) = markup.clone() {
        plain_request = plain_request.reply_markup(existing_markup);
    }

    match plain_request.await {
        Ok(message) => return Ok(message),
        Err(err) => {
            tracing::debug!(
                "Failed to edit Telegram message {} with plain text, sending new message: {}",
                source_message_id.0,
                err
            );
        }
    }

    let sent = send_payload(bot, chat_id, thread_id, payload, markup).await?;
    if sent.id != source_message_id
        && let Err(err) = bot.delete_message(chat_id, source_message_id).await
    {
        tracing::debug!(
            "Failed to delete Telegram message {} in chat {}: {}",
            source_message_id.0,
            chat_id.0,
            err
        );
    }
    Ok(sent)
}

#[derive(Clone)]
struct TelegramPayload {
    rich: Option<String>,
    plain: String,
}

impl TelegramPayload {
    fn from_markdown(markdown: &str) -> Self {
        let rich = {
            let rendered = markdown_to_telegram_html(markdown);
            let char_count = rendered.chars().count();
            if rendered.is_empty() || char_count > TELEGRAM_MESSAGE_LIMIT {
                if char_count > TELEGRAM_MESSAGE_LIMIT {
                    tracing::debug!(
                        "Telegram rich HTML ({} chars) exceeds limit ({}); falling back to plain text",
                        char_count,
                        TELEGRAM_MESSAGE_LIMIT
                    );
                }
                None
            } else {
                Some(rendered)
            }
        };

        let plain_source = markdown_to_plain_text(markdown);
        let plain = clamp_plain_message(&plain_source);

        Self { rich, plain }
    }
}

async fn send_payload(
    bot: &Bot,
    chat_id: ChatId,
    thread_id: Option<ThreadId>,
    payload: TelegramPayload,
    markup: Option<InlineKeyboardMarkup>,
) -> Result<Message, RequestError> {
    if let Some(rich) = payload.rich {
        let mut request = bot.send_message(chat_id, rich).parse_mode(ParseMode::Html);
        if let Some(thread_id) = thread_id {
            request = request.message_thread_id(thread_id);
        }
        if let Some(existing_markup) = markup.clone() {
            request = request.reply_markup(existing_markup);
        }

        match request.await {
            Ok(message) => return Ok(message),
            Err(err) => {
                tracing::warn!(
                    "Failed to send Telegram rich-text message, retrying as plain text: {}",
                    err
                );
            }
        }
    }

    let mut request = bot.send_message(chat_id, payload.plain);
    if let Some(thread_id) = thread_id {
        request = request.message_thread_id(thread_id);
    }
    if let Some(existing_markup) = markup {
        request = request.reply_markup(existing_markup);
    }

    request.await
}

fn append_block_break(out: &mut String) {
    if out.is_empty() {
        return;
    }

    if out.ends_with("\n\n") {
        return;
    }

    if out.ends_with('\n') {
        out.push('\n');
    } else {
        out.push_str("\n\n");
    }
}

pub(crate) fn clamp_plain_message(message: &str) -> String {
    let mut normalized = if message.trim().is_empty() {
        "(empty)".to_string()
    } else {
        message.to_string()
    };

    let current_len = normalized.chars().count();
    if current_len <= TELEGRAM_MESSAGE_LIMIT {
        return normalized;
    }

    normalized = normalized
        .chars()
        .take(TELEGRAM_MESSAGE_LIMIT.saturating_sub(1))
        .collect();
    normalized.push('…');
    normalized
}

#[cfg(test)]
mod tests {
    use super::{
        TELEGRAM_MESSAGE_LIMIT, clamp_plain_message, escape_telegram_html, markdown_to_plain_text,
        markdown_to_telegram_html, split_telegram_chunks,
    };

    #[test]
    fn markdown_common_syntax_to_telegram_html() {
        let markdown = "# Title\n\n- **Bold**\n- *Italic*\n- `Inline`\n\n```rust\nfn hi() {}\n```\n\n[Docs](https://example.com)";
        let html = markdown_to_telegram_html(markdown);

        assert!(html.contains("<b>Title</b>"));
        assert!(html.contains("<b>Bold</b>"));
        assert!(html.contains("<i>Italic</i>"));
        assert!(html.contains("<code>Inline</code>"));
        assert!(html.contains("<pre><code>fn hi() {}"));
        assert!(html.contains("<a href=\"https://example.com\">Docs</a>"));
        assert!(html.contains("- "));
    }

    #[test]
    fn escape_telegram_html_escapes_reserved_characters() {
        let escaped = escape_telegram_html("<tag>&\"'>");
        assert_eq!(escaped, "&lt;tag&gt;&amp;&quot;&#39;&gt;");
    }

    #[test]
    fn split_telegram_chunks_handles_boundaries_and_unicode() {
        assert!(split_telegram_chunks("", 8, 2).is_empty());

        let chunks = split_telegram_chunks("abcdefghij", 6, 2);
        assert_eq!(chunks, vec!["abcd", "efgh", "ij"]);

        let reserved_chunks = split_telegram_chunks("abcdef", 5, 10);
        assert_eq!(reserved_chunks, vec!["a", "b", "c", "d", "e", "f"]);

        let unicode_chunks = split_telegram_chunks("你好世界", 4, 1);
        assert_eq!(unicode_chunks, vec!["你好世", "界"]);
    }

    #[test]
    fn plain_text_fallback_is_readable_and_has_no_html_tags() {
        let markdown = "# Header\n\n- Item\n\n[Docs](https://example.com)";
        let plain = markdown_to_plain_text(markdown);

        assert!(plain.contains("Header"));
        assert!(plain.contains("- Item"));
        assert!(plain.contains("Docs (https://example.com)"));
        assert!(!plain.contains("<b>"));
        assert!(!plain.contains("<a "));
    }

    #[test]
    fn split_telegram_chunks_defaults_to_service_limit() {
        let long = "x".repeat(TELEGRAM_MESSAGE_LIMIT + 10);
        let chunks = split_telegram_chunks(&long, TELEGRAM_MESSAGE_LIMIT, 20);
        assert!(chunks.len() >= 2);
    }

    #[test]
    fn clamp_plain_message_returns_empty_sentinel_for_blank_input() {
        assert_eq!(clamp_plain_message(""), "(empty)");
        assert_eq!(clamp_plain_message("   "), "(empty)");
        assert_eq!(clamp_plain_message("\n\t"), "(empty)");
    }

    #[test]
    fn clamp_plain_message_passes_through_short_message_unchanged() {
        let msg = "Hello, world!";
        assert_eq!(clamp_plain_message(msg), msg);
    }

    #[test]
    fn clamp_plain_message_truncates_long_message_with_ellipsis() {
        let long = "a".repeat(TELEGRAM_MESSAGE_LIMIT + 100);
        let clamped = clamp_plain_message(&long);
        let char_count = clamped.chars().count();
        assert_eq!(char_count, TELEGRAM_MESSAGE_LIMIT);
        assert!(clamped.ends_with('…'));
    }
}
