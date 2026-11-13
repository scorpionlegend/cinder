//! The window chrome: a toolbar with back, forward and reload, an address
//! field, and a status strip along the bottom.
//!
//! Everything here is drawn by hand into the same pixel buffer the page is
//! blitted into. There is no widget toolkit and no text stack, so the layout is
//! a handful of rectangles and the text comes from the embedded bitmap font in
//! `font.rs`.
//!
//! The drawing helpers take explicit geometry rather than a struct because they
//! are called from tight per-pixel loops; the argument counts are deliberate.
#![allow(clippy::too_many_arguments)]

use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::font;

/// Toolbar height in logical pixels.
pub const TOOLBAR: f32 = 40.0;
/// Tab strip height in logical pixels.
pub const TABSTRIP: f32 = 32.0;
/// Status strip height in logical pixels.
pub const STATUS: f32 = 22.0;

const TOOLBAR_BG: u32 = 0x00f2f2f4;
const TABSTRIP_BG: u32 = 0x00e4e4ea;
const TAB_ACTIVE: u32 = 0x00f2f2f4;
const TAB_IDLE: u32 = 0x00d6d6dd;
const TOOLBAR_LINE: u32 = 0x00d8d8dd;
const BUTTON_HOVER: u32 = 0x00dedee4;
const BUTTON_GLYPH: u32 = 0x003a3a40;
const BUTTON_GLYPH_OFF: u32 = 0x00b4b4bc;
const FIELD_BG: u32 = 0x00ffffff;
const FIELD_LINE: u32 = 0x00c6c6cc;
const FIELD_LINE_FOCUS: u32 = 0x003b82f6;
const TEXT: u32 = 0x001f1f24;
const TEXT_DIM: u32 = 0x00707078;
const SELECT_BG: u32 = 0x003b82f6;
const STATUS_BG: u32 = 0x00e9e9ee;
const STATUS_TEXT: u32 = 0x0033333a;

/// What the pointer is over, in the chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Back,
    Forward,
    Reload,
    Url,
    Status,
    /// The body of a tab in the strip, by index.
    Tab(usize),
    /// The close box of a tab in the strip, by index.
    TabClose(usize),
    /// The `+` button that opens a new tab.
    NewTab,
}

/// One tab as the strip draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabChip {
    pub title: String,
    pub active: bool,
}

/// What the address field wants the shell to do after a key press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Edit {
    /// Nothing; the field consumed the key.
    None,
    /// The user pressed Enter; navigate to this text.
    Navigate(String),
    /// The user pressed Escape; drop the edit and restore the page URL.
    Cancel,
}

/// Chrome state: what to draw and what the pointer is over.
#[derive(Debug)]
pub struct Chrome {
    /// Device pixels per logical pixel.
    pub scale: f32,
    /// The page's current URL, as last reported by the engine.
    pub url: String,
    pub back: bool,
    pub forward: bool,
    /// Text for the status strip; empty hides nothing, the strip stays.
    pub status: String,
    pub hover: Option<Hit>,
    /// True while the address field has the keyboard.
    pub editing: bool,
    /// The text in the address field while editing.
    pub edit: String,
    /// Caret position in `edit`, as a byte index on a character boundary.
    pub caret: usize,
    /// The whole field is selected, so the first keystroke replaces it.
    pub select_all: bool,
    /// Blink phase for the caret.
    pub caret_on: bool,
    /// Tabs to draw in the strip, left to right.
    pub tabs: Vec<TabChip>,
}

impl Chrome {
    pub fn new(scale: f32) -> Self {
        Self {
            scale,
            url: String::new(),
            back: false,
            forward: false,
            status: String::new(),
            hover: None,
            editing: false,
            edit: String::new(),
            caret: 0,
            select_all: false,
            caret_on: true,
            tabs: Vec::new(),
        }
    }

    /// Tab strip height in device pixels.
    pub fn tabstrip_px(&self) -> u32 {
        (TABSTRIP * self.scale).round() as u32
    }

    /// Toolbar height in device pixels.
    pub fn toolbar_px(&self) -> u32 {
        (TOOLBAR * self.scale).round() as u32
    }

    /// Everything above the page: tab strip plus toolbar.
    pub fn chrome_px(&self) -> u32 {
        self.tabstrip_px() + self.toolbar_px()
    }

    /// Status strip height in device pixels.
    pub fn status_px(&self) -> u32 {
        (STATUS * self.scale).round() as u32
    }

    /// Integer scale for the bitmap font, so glyphs stay crisp.
    fn text_scale(&self) -> u32 {
        if self.scale >= 1.5 {
            2
        } else {
            1
        }
    }

    fn metrics(&self, width: u32) -> Metrics {
        let s = self.scale;
        let px = |v: f32| (v * s).round() as u32;
        let top = self.tabstrip_px();
        let toolbar = self.toolbar_px();
        let button = px(28.0);
        let gap = px(6.0);
        let margin = px(8.0);
        let field_h = px(26.0);
        let back = margin;
        let forward = back + button + gap;
        let reload = forward + button + gap;
        let field_x = reload + button + gap * 2;
        let field_w = width.saturating_sub(field_x + margin);
        // Tabs share the strip with the `+` button, capped so a single tab does
        // not stretch across the window.
        let new_tab_w = px(28.0);
        let avail = width.saturating_sub(margin * 2 + new_tab_w + gap);
        let count = self.tabs.len().max(1) as u32;
        let tab_w = (avail / count).min(px(220.0)).max(px(60.0));
        Metrics {
            top,
            toolbar,
            button,
            field_x,
            field_w,
            field_y: top + toolbar.saturating_sub(field_h) / 2,
            field_h,
            back,
            forward,
            reload,
            tab_w,
            new_tab: margin + tab_w * self.tabs.len() as u32 + gap,
        }
    }

    /// Which part of the chrome a point falls on, if any.
    pub fn hit(&self, x: f64, y: f64, width: u32) -> Option<Hit> {
        let m = self.metrics(width);
        let x = x as u32;
        let y = y as u32;
        if y < m.top {
            let margin = (8.0 * self.scale).round() as u32;
            if x < margin {
                return None;
            }
            let index = ((x - margin) / m.tab_w.max(1)) as usize;
            if index < self.tabs.len() {
                let within = (x - margin) % m.tab_w.max(1);
                let close = m.tab_w.saturating_sub((22.0 * self.scale).round() as u32);
                if within >= close {
                    return Some(Hit::TabClose(index));
                }
                return Some(Hit::Tab(index));
            }
            if x >= m.new_tab && x < m.new_tab + m.button {
                return Some(Hit::NewTab);
            }
            return None;
        }
        if y < m.top + m.toolbar {
            if x >= m.back && x < m.back + m.button {
                return Some(Hit::Back);
            }
            if x >= m.forward && x < m.forward + m.button {
                return Some(Hit::Forward);
            }
            if x >= m.reload && x < m.reload + m.button {
                return Some(Hit::Reload);
            }
            if x >= m.field_x && x < m.field_x + m.field_w {
                return Some(Hit::Url);
            }
            return None;
        }
        None
    }

    /// Whether a point is over the status strip.
    pub fn over_status(&self, y: f64, height: u32) -> bool {
        let strip = self.status_px();
        y as u32 >= height.saturating_sub(strip)
    }

    /// Start editing the address, with the whole URL selected.
    pub fn begin_edit(&mut self) {
        self.editing = true;
        self.edit = self.url.clone();
        self.caret = self.edit.len();
        self.select_all = true;
        self.caret_on = true;
    }

    /// Leave the address field, discarding the edit.
    pub fn cancel_edit(&mut self) {
        self.editing = false;
        self.select_all = false;
        self.edit.clear();
        self.caret = 0;
    }

    /// Apply a key press to the address field.
    pub fn key(&mut self, key: &Key, mods: ModifiersState) -> Edit {
        let ctrl = mods.control_key();
        match key {
            Key::Named(NamedKey::Enter) => {
                let text = self.edit.trim().to_string();
                self.cancel_edit();
                return Edit::Navigate(text);
            }
            Key::Named(NamedKey::Escape) => {
                self.cancel_edit();
                return Edit::Cancel;
            }
            Key::Named(NamedKey::Backspace) => {
                if self.select_all {
                    self.edit.clear();
                    self.caret = 0;
                    self.select_all = false;
                } else if let Some(prev) = self.prev_boundary() {
                    self.edit.replace_range(prev..self.caret, "");
                    self.caret = prev;
                }
            }
            Key::Named(NamedKey::Delete) => {
                if self.select_all {
                    self.edit.clear();
                    self.caret = 0;
                    self.select_all = false;
                } else if self.caret < self.edit.len() {
                    let next = self.next_boundary();
                    self.edit.replace_range(self.caret..next, "");
                }
            }
            Key::Named(NamedKey::ArrowLeft) => {
                self.select_all = false;
                self.caret = self.prev_boundary().unwrap_or(0);
            }
            Key::Named(NamedKey::ArrowRight) => {
                self.select_all = false;
                self.caret = self.next_boundary();
            }
            Key::Named(NamedKey::Home) => {
                self.select_all = false;
                self.caret = 0;
            }
            Key::Named(NamedKey::End) => {
                self.select_all = false;
                self.caret = self.edit.len();
            }
            Key::Character(text) => {
                if ctrl {
                    if text.eq_ignore_ascii_case("a") {
                        self.select_all = true;
                        self.caret = self.edit.len();
                    }
                    return Edit::None;
                }
                for ch in text.chars().filter(|c| !c.is_control()) {
                    if self.select_all {
                        self.edit.clear();
                        self.caret = 0;
                        self.select_all = false;
                    }
                    self.edit.insert(self.caret, ch);
                    self.caret += ch.len_utf8();
                }
            }
            _ => {}
        }
        self.caret_on = true;
        Edit::None
    }

    fn prev_boundary(&self) -> Option<usize> {
        if self.caret == 0 {
            return None;
        }
        let mut i = self.caret - 1;
        while i > 0 && !self.edit.is_char_boundary(i) {
            i -= 1;
        }
        Some(i)
    }

    fn next_boundary(&self) -> usize {
        if self.caret >= self.edit.len() {
            return self.edit.len();
        }
        let mut i = self.caret + 1;
        while i < self.edit.len() && !self.edit.is_char_boundary(i) {
            i += 1;
        }
        i
    }

    /// Draw the chrome over a `width` x `height` buffer of `0x00RRGGBB`.
    pub fn draw(&self, buf: &mut [u32], width: u32, height: u32) {
        let m = self.metrics(width);
        let ts = self.text_scale();
        let text_h = font::CELL_H * ts;
        let strip = self.status_px();

        fill(buf, width, 0, 0, width, m.top, TABSTRIP_BG);
        self.tab_strip(buf, width, height, m, ts);
        fill(buf, width, 0, m.top, width, m.top + m.toolbar, TOOLBAR_BG);
        fill(buf, width, 0, m.top + m.toolbar - 1, width, m.top + m.toolbar, TOOLBAR_LINE);

        self.button(buf, width, m.back, m, Hit::Back, |buf, width, x, y, s| {
            let mid = y + (m.button as i32) / 2;
            let apex = x + (9.0 * s).round() as i32;
            let tail = x + (15.0 * s).round() as i32;
            let reach = (5.0 * s).round() as i32;
            let color = if self.back { BUTTON_GLYPH } else { BUTTON_GLYPH_OFF };
            line(buf, width, tail, mid - reach, apex, mid, color, 2);
            line(buf, width, tail, mid + reach, apex, mid, color, 2);
        });
        self.button(buf, width, m.forward, m, Hit::Forward, |buf, width, x, y, s| {
            let mid = y + (m.button as i32) / 2;
            let apex = x + (19.0 * s).round() as i32;
            let tail = x + (13.0 * s).round() as i32;
            let reach = (5.0 * s).round() as i32;
            let color = if self.forward { BUTTON_GLYPH } else { BUTTON_GLYPH_OFF };
            line(buf, width, tail, mid - reach, apex, mid, color, 2);
            line(buf, width, tail, mid + reach, apex, mid, color, 2);
        });
        self.button(buf, width, m.reload, m, Hit::Reload, |buf, width, x, y, s| {
            let cx = x + (m.button as i32) / 2;
            let cy = y + (m.button as i32) / 2;
            let r = (6.0 * s).round() as i32;
            let t = ((2.0 * s).round() as i32).max(2);
            ring(buf, width, cx, cy, r, t, 300.0, 240.0, BUTTON_GLYPH);
            // Arrowhead on the arc's end, so the ring reads as a reload.
            let angle = 240f32.to_radians();
            let end_x = cx + (r as f32 * angle.cos()).round() as i32;
            let end_y = cy + (r as f32 * angle.sin()).round() as i32;
            let wing = (3.0 * s).round() as i32;
            line(buf, width, end_x - wing, end_y - wing, end_x + 1, end_y, BUTTON_GLYPH, t);
            line(buf, width, end_x - wing, end_y + wing, end_x + 1, end_y, BUTTON_GLYPH, t);
        });

        // Address field.
        let border = if self.editing { FIELD_LINE_FOCUS } else { FIELD_LINE };
        round_rect(buf, width, m.field_x, m.field_y, m.field_w, m.field_h, (6.0 * self.scale) as u32, FIELD_BG);
        round_outline(buf, width, m.field_x, m.field_y, m.field_w, m.field_h, (6.0 * self.scale) as u32, border);

        let pad = (7.0 * self.scale).round() as u32;
        let inner_x = m.field_x + pad;
        let inner_w = m.field_w.saturating_sub(pad * 2);
        let text_y = m.field_y + (m.field_h.saturating_sub(text_h)) / 2;
        let shown = if self.editing { self.edit.as_str() } else { self.url.as_str() };
        let color = if self.editing { TEXT } else { TEXT_DIM };
        if shown.is_empty() {
            if self.editing {
                // Nothing to show; the caret below is the only cue.
            } else {
                text(buf, width, height, inner_x, text_y, "about:blank", ts, TEXT_DIM, inner_x, inner_x + inner_w);
            }
        } else {
            let caret_x = inner_x + font::width(&shown[..self.caret.min(shown.len())], ts);
            let shift = if self.editing && caret_x > inner_x + inner_w {
                caret_x - (inner_x + inner_w)
            } else {
                0
            };
            let draw_x = inner_x.saturating_sub(shift);
            let clip_from = m.field_x + 1;
            let clip_to = m.field_x + m.field_w - 1;
            if self.editing && self.select_all {
                let end = draw_x + font::width(shown, ts);
                fill(buf, width, clip_from, text_y, end.min(clip_to), text_y + text_h, SELECT_BG);
                text(buf, width, height, draw_x, text_y, shown, ts, 0x00ffffff, clip_from, clip_to);
            } else {
                text(buf, width, height, draw_x, text_y, shown, ts, color, clip_from, clip_to);
            }
            if self.editing && self.caret_on && !self.select_all {
                let x = caret_x.saturating_sub(shift);
                if x >= clip_from && x < clip_to {
                    fill(buf, width, x, text_y, x + 1, text_y + text_h, TEXT);
                }
            }
        }

        // Status strip.
        if strip > 0 && height > strip {
            let top = height - strip;
            fill(buf, width, 0, top, width, height, STATUS_BG);
            fill(buf, width, 0, top, width, top + 1, TOOLBAR_LINE);
            if !self.status.is_empty() {
                let ty = top + (strip.saturating_sub(text_h)) / 2;
                let pad = (10.0 * self.scale).round() as u32;
                text(buf, width, height, pad, ty, &self.status, ts, STATUS_TEXT, pad, width);
            }
        }
    }

    fn button(
        &self,
        buf: &mut [u32],
        width: u32,
        x: u32,
        m: Metrics,
        hit: Hit,
        glyph: impl Fn(&mut [u32], u32, i32, i32, f32),
    ) {
        let y = m.top + m.toolbar.saturating_sub(m.button) / 2;
        if self.hover == Some(hit) {
            round_rect(buf, width, x, y, m.button, m.button, (6.0 * self.scale) as u32, BUTTON_HOVER);
        }
        glyph(buf, width, x as i32, y as i32, self.scale);
    }

    /// The tab strip: one chip per tab, then the `+` button.
    fn tab_strip(&self, buf: &mut [u32], width: u32, height: u32, m: Metrics, ts: u32) {
        let margin = (8.0 * self.scale).round() as u32;
        let pad = (7.0 * self.scale).round() as u32;
        let text_h = font::CELL_H * ts;
        let radius = (6.0 * self.scale) as u32;
        let inset = (4.0 * self.scale).round() as u32;
        let chip_h = m.top.saturating_sub(inset);
        for (index, tab) in self.tabs.iter().enumerate() {
            let x = margin + m.tab_w * index as u32;
            let w = m.tab_w.saturating_sub(2);
            let bg = if tab.active { TAB_ACTIVE } else { TAB_IDLE };
            round_rect(buf, width, x, inset, w, chip_h, radius, bg);
            let text_y = inset + (chip_h.saturating_sub(text_h)) / 2;
            let close_w = (22.0 * self.scale).round() as u32;
            let text_w = w.saturating_sub(pad * 2 + close_w);
            let color = if tab.active { TEXT } else { TEXT_DIM };
            text(
                buf,
                width,
                height,
                x + pad,
                text_y,
                &tab.title,
                ts,
                color,
                x + pad,
                x + pad + text_w,
            );
            // Close box: a small x, highlighted when hovered.
            let cx = x + w.saturating_sub(close_w) + close_w / 2;
            let cy = inset + chip_h / 2;
            if self.hover == Some(Hit::TabClose(index)) {
                let r = (7.0 * self.scale).round() as u32;
                round_rect(buf, width, cx - r, cy - r, r * 2, r * 2, r, BUTTON_HOVER);
            }
            let arm = (3.0 * self.scale).round() as i32;
            line(buf, width, cx as i32 - arm, cy as i32 - arm, cx as i32 + arm, cy as i32 + arm, BUTTON_GLYPH, 2);
            line(buf, width, cx as i32 - arm, cy as i32 + arm, cx as i32 + arm, cy as i32 - arm, BUTTON_GLYPH, 2);
        }
        // `+` for a new tab.
        let y = inset + chip_h.saturating_sub(m.button) / 2;
        if self.hover == Some(Hit::NewTab) {
            round_rect(buf, width, m.new_tab, y, m.button, m.button, radius, BUTTON_HOVER);
        }
        let cx = (m.new_tab + m.button / 2) as i32;
        let cy = (y + m.button / 2) as i32;
        let arm = (5.0 * self.scale).round() as i32;
        line(buf, width, cx - arm, cy, cx + arm, cy, BUTTON_GLYPH, 2);
        line(buf, width, cx, cy - arm, cx, cy + arm, BUTTON_GLYPH, 2);
    }
}

/// Pixel positions of the chrome's parts, in device pixels.
#[derive(Debug, Clone, Copy)]
struct Metrics {
    /// Tab strip height; the toolbar starts below it.
    top: u32,
    toolbar: u32,
    button: u32,
    field_x: u32,
    field_w: u32,
    field_y: u32,
    field_h: u32,
    back: u32,
    forward: u32,
    reload: u32,
    /// Width of one tab, and the x of the `+` button.
    tab_w: u32,
    new_tab: u32,
}

fn fill(buf: &mut [u32], width: u32, x0: u32, y0: u32, x1: u32, y1: u32, color: u32) {
    let width = width as usize;
    let (x0, x1) = (x0.min(x1) as usize, x0.max(x1) as usize);
    for y in y0..y1 {
        let row = y as usize * width;
        if row + x1 > buf.len() {
            break;
        }
        for px in &mut buf[row + x0..row + x1] {
            *px = color;
        }
    }
}

fn round_rect(buf: &mut [u32], width: u32, x: u32, y: u32, w: u32, h: u32, radius: u32, color: u32) {
    if w == 0 || h == 0 {
        return;
    }
    let r = radius.min(w / 2).min(h / 2);
    fill(buf, width, x + r, y, x + w - r, y + h, color);
    fill(buf, width, x, y + r, x + r, y + h - r, color);
    fill(buf, width, x + w - r, y + r, x + w, y + h - r, color);
    let r = r as i32;
    let (cx0, cy0) = ((x + r as u32) as i32, (y + r as u32) as i32);
    let (cx1, cy1) = ((x + w - r as u32) as i32, (y + h - r as u32) as i32);
    for dy in -r..r {
        for dx in -r..r {
            if dx * dx + dy * dy > r * r {
                continue;
            }
            for (cx, cy) in [(cx0, cy0), (cx1, cy0), (cx0, cy1), (cx1, cy1)] {
                let px = cx + dx;
                let py = cy + dy;
                if px >= 0 && py >= 0 && (px as u32) < width && (py as u32) < buf.len() as u32 / width {
                    buf[py as usize * width as usize + px as usize] = color;
                }
            }
        }
    }
}

fn round_outline(buf: &mut [u32], width: u32, x: u32, y: u32, w: u32, h: u32, radius: u32, color: u32) {
    if w < 2 || h < 2 {
        return;
    }
    let r = radius.min(w / 2).min(h / 2);
    fill(buf, width, x + r, y, x + w - r, y + 1, color);
    fill(buf, width, x + r, y + h - 1, x + w - r, y + h, color);
    fill(buf, width, x, y + r, x + 1, y + h - r, color);
    fill(buf, width, x + w - 1, y + r, x + w, y + h - r, color);
}

fn line(buf: &mut [u32], width: u32, x0: i32, y0: i32, x1: i32, y1: i32, color: u32, thickness: i32) {
    let steps = (x1 - x0).abs().max((y1 - y0).abs()).max(1);
    for i in 0..=steps {
        let x = x0 + (x1 - x0) * i / steps;
        let y = y0 + (y1 - y0) * i / steps;
        fill(
            buf,
            width,
            x.max(0) as u32,
            y.max(0) as u32,
            (x + thickness).max(0) as u32,
            (y + thickness).max(0) as u32,
            color,
        );
    }
}

fn ring(
    buf: &mut [u32],
    width: u32,
    cx: i32,
    cy: i32,
    r: i32,
    thickness: i32,
    from: f32,
    to: f32,
    color: u32,
) {
    for dy in -r..=r {
        for dx in -r..=r {
            let d2 = dx * dx + dy * dy;
            let outer = (r + thickness / 2) * (r + thickness / 2);
            let inner = (r - thickness / 2) * (r - thickness / 2);
            if d2 > outer || d2 < inner {
                continue;
            }
            let mut angle = (dy as f32).atan2(dx as f32).to_degrees();
            if angle < 0.0 {
                angle += 360.0;
            }
            let (from, to) = (from.rem_euclid(360.0), to.rem_euclid(360.0));
            let inside = if from <= to {
                angle >= from && angle <= to
            } else {
                angle >= from || angle <= to
            };
            if !inside {
                continue;
            }
            let (px, py) = (cx + dx, cy + dy);
            if px >= 0 && py >= 0 && (px as u32) < width && (py as u32) < buf.len() as u32 / width {
                buf[py as usize * width as usize + px as usize] = color;
            }
        }
    }
}


#[allow(clippy::too_many_arguments)]
fn text(
    buf: &mut [u32],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    value: &str,
    scale: u32,
    color: u32,
    clip_from: u32,
    clip_to: u32,
) {
    let mut pen = x as i32;
    for ch in value.chars() {
        let glyph = font::glyph(ch);
        let advance = font::advance(ch) * scale;
        for row in 0..font::CELL_H {
            for col in 0..font::CELL_W {
                let alpha = glyph[(row * font::CELL_W + col) as usize];
                if alpha == 0 {
                    continue;
                }
                let px = pen + (col * scale) as i32;
                let py = y + row * scale;
                if px < clip_from as i32 || px >= clip_to as i32 {
                    continue;
                }
                for sy in 0..scale {
                    for sx in 0..scale {
                        let (fx, fy) = (px + sx as i32, py as i32 + sy as i32);
                        if fx < 0 || fy >= height as i32 || fx >= width as i32 {
                            continue;
                        }
                        let index = fy as usize * width as usize + fx as usize;
                        if index >= buf.len() {
                            continue;
                        }
                        buf[index] = blend(buf[index], color, alpha);
                    }
                }
            }
        }
        pen += advance as i32;
    }
}

fn blend(dst: u32, src: u32, alpha: u8) -> u32 {
    let a = alpha as u32;
    let mix = |shift: u32| {
        let d = (dst >> shift) & 0xff;
        let s = (src >> shift) & 0xff;
        ((d * (255 - a) + s * a) / 255) & 0xff
    };
    (mix(16) << 16) | (mix(8) << 8) | mix(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chrome() -> Chrome {
        let mut chrome = Chrome::new(1.0);
        chrome.url = "https://example.com/".to_string();
        chrome
    }

    #[test]
    fn hit_test_maps_the_toolbar() {
        let chrome = chrome();
        let width = 1000;
        assert_eq!(chrome.hit(10.0, 52.0, width), Some(Hit::Back));
        assert_eq!(chrome.hit(44.0, 52.0, width), Some(Hit::Forward));
        assert_eq!(chrome.hit(78.0, 52.0, width), Some(Hit::Reload));
        assert_eq!(chrome.hit(400.0, 52.0, width), Some(Hit::Url));
        assert_eq!(chrome.hit(400.0, 200.0, width), None);
        assert!(chrome.over_status(630.0, 640));
        assert!(!chrome.over_status(200.0, 640));
    }

    #[test]
    fn hit_test_maps_the_tab_strip() {
        let mut chrome = chrome();
        chrome.tabs = vec![
            TabChip { title: "One".into(), active: true },
            TabChip { title: "Two".into(), active: false },
        ];
        let width = 1000;
        assert_eq!(chrome.hit(20.0, 10.0, width), Some(Hit::Tab(0)));
        assert_eq!(chrome.hit(240.0, 10.0, width), Some(Hit::Tab(1)));
        // The close box sits at the right edge of each chip; a single tab is
        // capped at 220 logical pixels.
        let tab_w = 220;
        assert_eq!(
            chrome.hit((8 + tab_w - 5) as f64, 10.0, width),
            Some(Hit::TabClose(0))
        );
        assert_eq!(chrome.hit(4.0, 10.0, width), None);
    }

    #[test]
    fn tab_strip_grows_the_chrome() {
        let chrome = Chrome::new(1.0);
        assert_eq!(chrome.tabstrip_px(), 32);
        assert_eq!(chrome.chrome_px(), 72);
    }

    #[test]
    fn typing_replaces_the_selected_url() {
        let mut chrome = chrome();
        chrome.begin_edit();
        assert!(chrome.select_all);
        let edit = chrome.key(&Key::Character("cinder.dev".into()), ModifiersState::empty());
        assert_eq!(edit, Edit::None);
        assert_eq!(chrome.edit, "cinder.dev");
        assert_eq!(chrome.caret, "cinder.dev".len());
        assert!(!chrome.select_all);
    }

    #[test]
    fn enter_navigates_and_escape_cancels() {
        let mut chrome = chrome();
        chrome.begin_edit();
        chrome.key(&Key::Character("a".into()), ModifiersState::empty());
        assert_eq!(
            chrome.key(&Key::Named(NamedKey::Enter), ModifiersState::empty()),
            Edit::Navigate("a".to_string())
        );
        assert!(!chrome.editing);

        chrome.begin_edit();
        assert_eq!(
            chrome.key(&Key::Named(NamedKey::Escape), ModifiersState::empty()),
            Edit::Cancel
        );
        assert!(!chrome.editing);
        assert!(chrome.edit.is_empty());
    }

    #[test]
    fn backspace_and_arrows_walk_characters() {
        let mut chrome = chrome();
        chrome.begin_edit();
        chrome.select_all = false;
        chrome.edit = "abc".to_string();
        chrome.caret = 3;
        chrome.key(&Key::Named(NamedKey::Backspace), ModifiersState::empty());
        assert_eq!(chrome.edit, "ab");
        assert_eq!(chrome.caret, 2);
        chrome.key(&Key::Named(NamedKey::ArrowLeft), ModifiersState::empty());
        assert_eq!(chrome.caret, 1);
        chrome.key(&Key::Named(NamedKey::Delete), ModifiersState::empty());
        assert_eq!(chrome.edit, "a");
        assert_eq!(chrome.caret, 1);
    }

    #[test]
    fn drawing_stays_inside_the_buffer() {
        let mut chrome = chrome();
        chrome.status = "Cinder 0.1.0-pre.6 is available".to_string();
        chrome.hover = Some(Hit::Back);
        chrome.tabs = vec![
            TabChip { title: "Cinder".into(), active: true },
            TabChip { title: "Example".into(), active: false },
        ];
        chrome.begin_edit();
        let (width, height) = (320u32, 200u32);
        let mut buffer = vec![0u32; (width * height) as usize];
        chrome.draw(&mut buffer, width, height);
        assert!(buffer.contains(&TOOLBAR_BG));
        assert!(buffer.contains(&STATUS_BG));
    }
}
