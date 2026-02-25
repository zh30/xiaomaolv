use std::time::{Duration, Instant};

use super::*;

pub(super) struct TelegramStreamSink<'a> {
    sender: &'a TelegramSender,
    chat_id: i64,
    message_thread_id: Option<i64>,
    reply_to_message_id: Option<i64>,
    prefer_draft: bool,
    draft_supported: bool,
    draft_message_id: String,
    min_edit_interval: Duration,
    message_id: Option<i64>,
    tail_message_ids: Vec<i64>,
    pending_text: String,
    rendered_parts: Vec<TelegramRenderedText>,
    last_push: Option<Instant>,
}

impl<'a> TelegramStreamSink<'a> {
    pub(super) fn new(
        sender: &'a TelegramSender,
        chat_id: i64,
        message_thread_id: Option<i64>,
        reply_to_message_id: Option<i64>,
        prefer_draft: bool,
        min_edit_interval: Duration,
    ) -> Self {
        Self {
            sender,
            chat_id,
            message_thread_id,
            reply_to_message_id,
            prefer_draft,
            draft_supported: message_thread_id.is_some() && reply_to_message_id.is_none(),
            draft_message_id: build_draft_message_id(chat_id, message_thread_id),
            min_edit_interval,
            message_id: None,
            tail_message_ids: Vec::new(),
            pending_text: String::new(),
            rendered_parts: Vec::new(),
            last_push: None,
        }
    }

    pub(super) async fn finalize(&mut self, full_text: &str) -> anyhow::Result<()> {
        if full_text.is_empty() {
            return Ok(());
        }

        self.pending_text = full_text.to_string();
        let parts = render_telegram_text_parts(&self.pending_text, TELEGRAM_MAX_TEXT_CHARS);
        if !parts.is_empty() && parts == self.rendered_parts {
            return Ok(());
        }

        if self.prefer_draft && self.draft_supported && self.message_id.is_none() {
            if let Err(err) = self
                .sender
                .send_message(
                    self.chat_id,
                    self.message_thread_id,
                    self.reply_to_message_id,
                    full_text,
                )
                .await
            {
                warn!(error = %err, "telegram stream final send after draft failed, fallback to send/edit");
                self.draft_supported = false;
            } else {
                self.rendered_parts = parts;
                self.last_push = Some(Instant::now());
                return Ok(());
            }
        }

        if let Err(err) = self.flush(true).await {
            warn!(error = %err, "telegram stream finalize failed, fallback to sendMessage");
            self.sender
                .send_message(
                    self.chat_id,
                    self.message_thread_id,
                    self.reply_to_message_id,
                    full_text,
                )
                .await
                .context("telegram stream fallback sendMessage failed")?;
            self.rendered_parts =
                render_telegram_text_parts(&self.pending_text, TELEGRAM_MAX_TEXT_CHARS);
            self.last_push = Some(Instant::now());
        }

        Ok(())
    }

    pub(super) async fn flush(&mut self, force: bool) -> anyhow::Result<()> {
        if self.pending_text.is_empty() {
            return Ok(());
        }

        if !force
            && let Some(last) = self.last_push
            && last.elapsed() < self.min_edit_interval
        {
            return Ok(());
        }

        let parts = render_telegram_text_parts(&self.pending_text, TELEGRAM_MAX_TEXT_CHARS);
        if parts.is_empty() || parts == self.rendered_parts {
            return Ok(());
        }

        if self.prefer_draft
            && self.draft_supported
            && self.message_id.is_none()
            && self.tail_message_ids.is_empty()
        {
            match self
                .sender
                .send_message_draft(
                    self.chat_id,
                    self.message_thread_id,
                    self.reply_to_message_id,
                    &self.draft_message_id,
                    &self.pending_text,
                )
                .await
            {
                Ok(_) => {
                    self.rendered_parts = parts;
                    self.last_push = Some(Instant::now());
                    return Ok(());
                }
                Err(err) => {
                    warn!(error = %err, "telegram sendMessageDraft failed, fallback to send/edit stream");
                    self.draft_supported = false;
                }
            }
        }

        if let Some(message_id) = self.message_id {
            if self.rendered_parts.first() != Some(&parts[0]) {
                self.sender
                    .edit_rendered_message(self.chat_id, message_id, &parts[0])
                    .await
                    .context("failed to edit telegram stream message")?;
            }
        } else {
            let message_id = self
                .sender
                .send_rendered_message_with_id(
                    self.chat_id,
                    self.message_thread_id,
                    self.reply_to_message_id,
                    &parts[0],
                )
                .await
                .context("failed to send telegram stream message")?;
            self.message_id = Some(message_id);
        }

        for (i, rendered_tail) in parts.iter().enumerate().skip(1) {
            let tail_idx = i - 1;
            if let Some(msg_id) = self.tail_message_ids.get(tail_idx).copied() {
                if self.rendered_parts.get(i) != Some(rendered_tail) {
                    self.sender
                        .edit_rendered_message(self.chat_id, msg_id, rendered_tail)
                        .await
                        .context("failed to edit telegram stream tail message")?;
                }
            } else {
                let tail_id = self
                    .sender
                    .send_rendered_message_with_id(
                        self.chat_id,
                        self.message_thread_id,
                        None,
                        rendered_tail,
                    )
                    .await
                    .context("failed to send telegram stream tail message")?;
                self.tail_message_ids.push(tail_id);
            }
        }

        self.rendered_parts = parts;
        self.last_push = Some(Instant::now());
        Ok(())
    }
}

#[async_trait]
impl StreamSink for TelegramStreamSink<'_> {
    async fn on_delta(&mut self, delta: &str) -> anyhow::Result<()> {
        if delta.is_empty() {
            return Ok(());
        }

        self.pending_text.push_str(delta);
        if let Err(err) = self.flush(false).await {
            warn!(error = %err, "telegram stream flush failed, continue buffering");
        }
        Ok(())
    }
}
