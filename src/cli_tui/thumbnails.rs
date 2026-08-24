use super::widgets::*;
use super::*;
use image::imageops::FilterType;

impl InteractiveTui {
    pub(in crate::cli_tui) fn ensure_thumbnail_requested(&mut self, url: Option<&str>) {
        let Some(url) = url.filter(|url| !url.is_empty()) else {
            return;
        };
        if !self.terminal_images_enabled() {
            return;
        }
        if self.thumbnail_requests.contains(url) || self.thumbnail_assets.contains_key(url) {
            return;
        }

        let url = url.to_string();
        self.thumbnail_requests.insert(url.clone());
        self.thumbnail_assets
            .insert(url.clone(), ThumbnailAsset::Loading);
        spawn_thumbnail_load(self.background_tx.clone(), url);
    }

    pub(in crate::cli_tui) fn thumbnail_source(&self, url: &str) -> Option<DynamicImage> {
        match self.thumbnail_assets.get(url) {
            Some(ThumbnailAsset::Ready(image)) => Some(image.clone()),
            _ => None,
        }
    }

    pub(in crate::cli_tui) fn thumbnail_failed(&self, url: &str) -> bool {
        matches!(
            self.thumbnail_assets.get(url),
            Some(ThumbnailAsset::Failed(message)) if !message.is_empty()
        )
    }

    pub(in crate::cli_tui) fn thumbnail_font_size(&self) -> (u16, u16) {
        self.thumbnail_picker
            .as_ref()
            .map(|picker| {
                let font = picker.font_size();
                (font.width, font.height)
            })
            .unwrap_or((10, 20))
    }

    pub(in crate::cli_tui) fn square_box_width_for_height(&self, outer_height: u16) -> u16 {
        if outer_height == 0 {
            return 0;
        }

        let (font_width, font_height) = self.thumbnail_font_size();
        let inner_height = outer_height.saturating_sub(2).max(1);
        let inner_width =
            ((inner_height as f64 * font_height as f64 / font_width as f64).round() as u16).max(1);
        inner_width.saturating_add(2)
    }

    pub(in crate::cli_tui) fn square_thumbnail_rect(&self, area: Rect) -> Rect {
        if area.width == 0 || area.height == 0 {
            return area;
        }

        let (font_width, font_height) = self.thumbnail_font_size();

        let pixel_width = area.width as f64 * font_width as f64;
        let pixel_height = area.height as f64 * font_height as f64;

        let (target_width, target_height) = if pixel_width > pixel_height {
            let width = ((pixel_height / font_width as f64).round() as u16)
                .max(1)
                .min(area.width);
            (width, area.height)
        } else {
            let height = ((pixel_width / font_height as f64).round() as u16)
                .max(1)
                .min(area.height);
            (area.width, height)
        };

        Rect::new(
            area.x + area.width.saturating_sub(target_width) / 2,
            area.y + area.height.saturating_sub(target_height) / 2,
            target_width,
            target_height,
        )
    }

    pub(in crate::cli_tui) fn render_protocol_thumbnail(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        slot: ThumbnailSlot,
        url: &str,
        resize: Resize,
    ) -> bool {
        if area.width == 0 || area.height == 0 || !self.terminal_images_enabled() {
            return false;
        }

        let Some(source) = self.thumbnail_source(url) else {
            return false;
        };
        self.render_protocol_thumbnail_source(frame, area, slot, url, source, resize)
    }

    pub(in crate::cli_tui) fn render_protocol_thumbnail_source(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        slot: ThumbnailSlot,
        key: &str,
        source: DynamicImage,
        resize: Resize,
    ) -> bool {
        if area.width == 0 || area.height == 0 || !self.terminal_images_enabled() {
            return false;
        }

        let needs_init = self
            .thumbnail_slots
            .get(&slot)
            .map(|state| state.key.as_str() != key)
            .unwrap_or(true);

        if needs_init {
            let Some(picker) = self.thumbnail_picker.clone() else {
                return false;
            };
            self.thumbnail_slots.insert(
                slot,
                ThumbnailSlotState {
                    key: key.to_string(),
                    protocol: picker.new_resize_protocol(source),
                },
            );
        }

        let encoding_result = if let Some(slot_state) = self.thumbnail_slots.get_mut(&slot) {
            frame.render_stateful_widget(
                StatefulImage::<StatefulProtocol>::default().resize(resize),
                area,
                &mut slot_state.protocol,
            );
            slot_state.protocol.last_encoding_result()
        } else {
            None
        };

        if let Some(Err(err)) = encoding_result {
            self.status_line = format!("Thumbnail render failed: {err}");
        }

        true
    }

    pub(in crate::cli_tui) fn render_cover_thumbnail(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        slot: ThumbnailSlot,
        url: Option<&str>,
        fallback_initials: String,
        theme: &TuiTheme,
        accent: bool,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let failed = url.is_some_and(|url| self.thumbnail_failed(url));
        let border = if accent && !failed {
            theme.accent
        } else {
            theme.border_subtle
        };
        let background = if accent && !failed {
            theme.accent_fill
        } else {
            theme.content_alt_bg
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border))
            .style(Style::default().bg(background));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let image_rect = self.square_thumbnail_rect(inner);

        if let Some(url) = url {
            self.ensure_thumbnail_requested(Some(url));
            if self.render_protocol_thumbnail(frame, image_rect, slot, url, Resize::Fit(None)) {
                return;
            }
        } else {
            self.thumbnail_slots.remove(&slot);
        }

        render_cover_placeholder(frame, image_rect, fallback_initials, theme);
    }

    pub(in crate::cli_tui) fn render_home_card_thumbnail(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        visible_area: Rect,
        index: usize,
        card: &HomeCard,
        theme: &TuiTheme,
    ) {
        if area.width == 0
            || area.height == 0
            || visible_area.width == 0
            || visible_area.height == 0
        {
            return;
        }

        let image_rect = self.square_thumbnail_rect(area);
        let Some(visible_rect) = intersect_rect(image_rect, visible_area) else {
            return;
        };
        let url = home_card_thumbnail_url(card);
        let failed = url.is_some_and(|url| self.thumbnail_failed(url));
        if let Some(url) = url {
            self.ensure_thumbnail_requested(Some(url));
            if let Some((source_key, source)) =
                self.thumbnail_source_for_visible_rect(url, image_rect, visible_rect)
            {
                if self.render_protocol_thumbnail_source(
                    frame,
                    visible_rect,
                    ThumbnailSlot::HomeCard(index),
                    &source_key,
                    source,
                    Resize::Fit(None),
                ) {
                    return;
                }
            }
        } else {
            self.thumbnail_slots.remove(&ThumbnailSlot::HomeCard(index));
        }

        render_home_thumbnail_fallback(
            frame,
            visible_rect,
            placeholder_initials(&card.title, &card.subtitle),
            theme,
            matches!(card.style, HomeCardStyle::Featured) && !failed,
        );
    }

    pub(in crate::cli_tui) fn thumbnail_source_for_visible_rect(
        &self,
        url: &str,
        full_rect: Rect,
        visible_rect: Rect,
    ) -> Option<(String, DynamicImage)> {
        let source = self.thumbnail_source(url)?;
        if visible_rect == full_rect {
            return Some((url.to_string(), source));
        }

        let (font_width, font_height) = self.thumbnail_font_size();
        let full_width = (full_rect.width as u32).saturating_mul(font_width as u32);
        let full_height = (full_rect.height as u32).saturating_mul(font_height as u32);
        let visible_width = (visible_rect.width as u32).saturating_mul(font_width as u32);
        let visible_height = (visible_rect.height as u32).saturating_mul(font_height as u32);
        if full_width == 0 || full_height == 0 || visible_width == 0 || visible_height == 0 {
            return None;
        }

        let x = visible_rect
            .x
            .saturating_sub(full_rect.x)
            .saturating_mul(font_width) as u32;
        let y = visible_rect
            .y
            .saturating_sub(full_rect.y)
            .saturating_mul(font_height) as u32;

        let scaled = source.resize_exact(full_width, full_height, FilterType::Nearest);
        let crop_width = visible_width.min(scaled.width().saturating_sub(x));
        let crop_height = visible_height.min(scaled.height().saturating_sub(y));
        if crop_width == 0 || crop_height == 0 {
            return None;
        }

        let key = format!(
            "{url}#{}x{}@{},{}:{}x{}",
            full_rect.width, full_rect.height, x, y, visible_rect.width, visible_rect.height
        );
        Some((key, scaled.crop_imm(x, y, crop_width, crop_height)))
    }
}

pub(in crate::cli_tui) fn spawn_thumbnail_load(tx: mpsc::Sender<TuiBackgroundEvent>, url: String) {
    std::thread::spawn(move || {
        let result = fetch_thumbnail_bytes(&url)
            .ok_or_else(|| format!("Could not fetch thumbnail bytes from {url}"));
        let _ = tx.send(TuiBackgroundEvent::ThumbnailLoaded { url, result });
    });
}

pub(in crate::cli_tui) fn center_crop_square(image: DynamicImage) -> DynamicImage {
    let width = image.width();
    let height = image.height();
    let size = width.min(height);
    if size == 0 || width == height {
        return image;
    }

    let x = (width.saturating_sub(size)) / 2;
    let y = (height.saturating_sub(size)) / 2;
    image.crop_imm(x, y, size, size)
}

pub(in crate::cli_tui) fn thumbnail_protocol_label(protocol: ProtocolType) -> &'static str {
    match protocol {
        ProtocolType::Halfblocks => "fallback",
        ProtocolType::Sixel => "sixel",
        ProtocolType::Kitty => "kitty",
        ProtocolType::Iterm2 => "iterm2",
    }
}
