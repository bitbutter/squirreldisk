//! The AI tab of the side panel (SquirrelDisk-AI fork).
//!
//! Trust model: the model's suggestions are data. Paths are validated against
//! the live tree; valid ones become collector candidates through the app's
//! existing `Session::collect`, so every deletion still passes the app's
//! safety checks and its confirm dialog (DECISIONS.md, DEC-AI-002). Nothing
//! the model says is ever executed; suggested commands are copy-only.

use super::app::{App, Modal, Target};
use super::theme::{with_alpha, Theme};
use super::widgets::{self, bold, cr, font, BtnStyle, Icon};
use crate::ai::{self, Msg, Phase};
use crate::i18n::tr;
use crate::tree::fmt_size;
use eframe::egui::{self, Align2, Color32, FontId, Id, Pos2, Rect, Sense, Stroke, Vec2};

/// Panel background + border, shared by the file list and the AI tab.
pub fn frame(ui: &egui::Ui, panel: Rect, theme: &Theme) {
    let p = ui.painter().clone();
    widgets::shadow(&p, panel, 18.0, 0.5, theme);
    p.rect_filled(panel, cr(12.0), with_alpha(theme.surface, if theme.dark { 0.72 } else { 0.9 }));
    p.rect_stroke(panel, cr(12.0), Stroke::new(1.0, with_alpha(theme.stroke, 0.6)), egui::StrokeKind::Inside);
}

/// Tab strip at the top of the side panel: FILES | AI.
pub fn tabs(app: &mut App, ui: &mut egui::Ui, panel: Rect, si: usize) {
    let theme = app.theme.clone();
    let strip = Rect::from_min_size(panel.min, Vec2::new(panel.width(), 26.0));
    let p = ui.painter().clone();
    p.line_segment([strip.left_bottom(), strip.right_bottom()], Stroke::new(1.0, with_alpha(theme.stroke, 0.6)));
    let mut clicked: Option<bool> = None;
    for (k, (label, is_ai)) in [("FILES", false), ("AI", true)].into_iter().enumerate() {
        let active = is_ai == app.sessions[si].ai.open;
        let g = p.layout_no_wrap(label.to_string(), bold(10.5), Color32::WHITE);
        let w = g.size().x + 18.0;
        let r = Rect::from_min_size(Pos2::new(strip.left() + 18.0 + k as f32 * 46.0, strip.center().y - 9.0), Vec2::new(w, 18.0));
        let resp = ui.interact(r, Id::new(("panel-tab", k)), Sense::click());
        let col = if active { theme.text } else if resp.hovered() { theme.text_dim } else { theme.text_faint };
        p.text(Pos2::new(r.left(), r.center().y), Align2::LEFT_CENTER, label, bold(10.5), col);
        if active {
            p.rect_filled(Rect::from_min_size(Pos2::new(r.left(), r.bottom() + 1.0), Vec2::new(w - 8.0, 2.0)), cr(1.0), theme.accent);
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.clicked() {
            clicked = Some(is_ai);
        }
    }
    // unread dot: local teaser waiting on the AI tab
    {
        let s = &mut app.sessions[si];
        if !s.ai.open && s.ai.teaser.is_some() && !s.ai.teaser_seen {
            p.circle_filled(Pos2::new(strip.left() + 18.0 + 46.0 + 26.0, strip.center().y), 3.5, theme.warn);
        }
        match clicked {
            Some(true) => {
                s.ai.open = true;
                s.ai.teaser_seen = true;
            }
            Some(false) => s.ai.open = false,
            None => {}
        }
    }
}

/// One suggestion row, resolved against the live tree.
pub struct PanelItemView {
    pub path: String,
    pub reason: String,
    pub node: Option<u32>,
    pub size: u64,
    pub collected: bool,
}

impl App {
    // -- call orchestration -------------------------------------------------

    fn ai_call(&mut self, si: usize, system: String, temperature: f32, ctx: &egui::Context) {
        let s = &mut self.sessions[si];
        let mut msgs = vec![Msg { role: "system", content: system }];
        msgs.extend(s.ai.history.iter().cloned());
        *s.ai.slot.lock().unwrap() = None;
        s.ai.phase = Phase::Busy;
        s.ai.error = None;
        ai::spawn_chat(
            &self.settings.ai_endpoint,
            self.settings.ai_api_key.trim(),
            &self.settings.ai_model,
            &msgs,
            temperature,
            s.ai.slot.clone(),
            ctx.clone(),
        );
    }

    pub fn ai_analyze(&mut self, si: usize, ctx: &egui::Context) {
        let dossier = {
            let s = &self.sessions[si];
            let t = s.tree.read().unwrap();
            let free = if let Target::Volume(v) = &s.target {
                if s.free_space > 0 {
                    Some(s.free_space)
                } else {
                    crate::scan::platform::volume_space(std::path::Path::new(&v.mount)).map(|x| x.1)
                }
            } else {
                None
            };
            ai::dossier(&t, free)
        };
        let s = &mut self.sessions[si];
        s.ai.dossier = dossier.clone();
        s.ai.history = vec![ai::user(dossier)];
        s.ai.triage = None;
        s.ai.analyzed = false;
        s.ai.follow_ups.clear();
        s.ai.scroll = 0.0;
        self.ai_call(si, ai::analyze_system_prompt(), 0.2, ctx);
    }

    fn ai_send_chat(&mut self, si: usize, ctx: &egui::Context) {
        let text = {
            let s = &mut self.sessions[si];
            let t = s.ai.input.trim().to_string();
            s.ai.input.clear();
            t
        };
        if text.is_empty() {
            return;
        }
        self.sessions[si].ai.history.push(ai::user(text));
        self.ai_call(si, ai::chat_system_prompt(), 0.3, ctx);
    }

    fn ai_send_expansions(&mut self, si: usize, ctx: &egui::Context) {
        let paths: Vec<String> = std::mem::take(&mut self.sessions[si].ai.follow_ups);
        if paths.is_empty() {
            return;
        }
        let mut text = String::new();
        {
            let s = &self.sessions[si];
            let t = s.tree.read().unwrap();
            for p in &paths {
                match ai::expansion_of(&t, p) {
                    Some(x) => {
                        text.push_str(&x);
                        text.push_str("\n\n");
                    }
                    None => text.push_str(&format!("EXPANSION of {p}: not in the scan (changed or mistyped).\n\n")),
                }
            }
        }
        self.sessions[si].ai.history.push(ai::user(text));
        self.ai_call(si, ai::chat_system_prompt(), 0.3, ctx);
    }

    // -- the panel ----------------------------------------------------------

    pub fn ai_panel_body(&mut self, ui: &mut egui::Ui, panel: Rect, si: usize) {
        let theme = self.theme.clone();
        let ctx = ui.ctx().clone();
        let configured = !self.settings.ai_api_key.trim().is_empty();
        let scanning = self.sessions[si].is_scanning();

        // harvest a finished worker call
        if self.sessions[si].ai.phase == Phase::Busy {
            ctx.request_repaint();
            let slot = self.sessions[si].ai.slot.clone();
            let taken = { slot.lock().unwrap().take() };
            if let Some(r) = taken {
                let s = &mut self.sessions[si];
                match r {
                    Ok(reply) => {
                        if !s.ai.analyzed {
                            s.ai.analyzed = true;
                            if let Some(t) = ai::parse_triage(&reply) {
                                s.ai.follow_ups = t.expand.clone();
                                s.ai.triage = Some(t);
                            }
                        } else {
                            let (text, expand) = ai::take_expand_request(&reply);
                            if let Some(p) = expand {
                                s.ai.follow_ups.push(p);
                            }
                            s.ai.history.push(ai::assistant(text));
                        }
                        s.ai.phase = Phase::Done;
                    }
                    Err(e) => {
                        s.ai.error = Some(e);
                        s.ai.phase = Phase::Error;
                    }
                }
            }
        }

        let mut action: Option<u8> = None; // 0 analyze, 1 send, 2 retry, 3 open settings, 4 expand, 5 toggle verify
        let mut collect: Vec<u32> = Vec::new();
        let mut copy_cmd: Option<String> = None;

        if !configured {
            let inner = Rect::from_min_max(Pos2::new(panel.left() + 18.0, panel.top() + 16.0 + 26.0), Pos2::new(panel.right() - 18.0, panel.bottom() - 16.0));
            let mut p = ui.painter().clone();
            p.set_clip_rect(panel);
            p.text(Pos2::new(inner.left(), inner.top() + 8.0), Align2::LEFT_TOP, tr("AI advice"), bold(15.0), theme.text);
            let body = tr("Ask a language model what should go. It reads folder names and sizes (never file contents) and sorts candidates into what is safe to delete, what needs a check, and what only you can judge.");
            let g = p.layout(body.to_string(), font(12.5), theme.text_dim, inner.width());
            let gh = g.size().y;
            p.galley(Pos2::new(inner.left(), inner.top() + 36.0), g, theme.text_dim);
            let note = tr("When you ask, the folder names and sizes of this scan are sent to the endpoint configured in Settings.");
            let g2 = p.layout(note.to_string(), font(11.0), theme.text_faint, inner.width());
            p.galley(Pos2::new(inner.left(), inner.top() + 36.0 + gh + 14.0), g2, theme.text_faint);
            let b = Rect::from_min_size(Pos2::new(inner.left(), panel.bottom() - 60.0), Vec2::new(170.0, 34.0));
            if widgets::button(ui, b, Id::new(("ai-settings", si)), tr("Open Settings"), Some(Icon::Gear), BtnStyle::Primary, &theme).clicked() {
                action = Some(3);
            }
        } else {
            let body = Rect::from_min_max(Pos2::new(panel.left() + 18.0, panel.top() + 16.0 + 26.0), Pos2::new(panel.right() - 18.0, panel.bottom() - 16.0 - 44.0));
            let input = Rect::from_min_size(Pos2::new(body.left(), panel.bottom() - 16.0 - 32.0), Vec2::new(body.width() - 48.0, 32.0));
            let send_r = Rect::from_min_size(Pos2::new(input.right() + 8.0, input.top()), Vec2::new(40.0, 32.0));

            // scroll bookkeeping (same pattern as the settings modal)
            if ui.rect_contains_pointer(body) {
                self.sessions[si].ai.scroll -= ui.input(|i| i.smooth_scroll_delta.y);
            }
            let max_scroll = (self.sessions[si].ai.content_h - body.height()).max(0.0);
            self.sessions[si].ai.scroll = self.sessions[si].ai.scroll.clamp(0.0, max_scroll);
            let top = body.top() - self.sessions[si].ai.scroll;

            // gather view data under one tree lock
            let (teaser, triage_view, history, phase, follow_ups, verify_open) = {
                let s = &self.sessions[si];
                let t = s.tree.read().unwrap();
                let mk = |items: &[ai::Item]| -> Vec<PanelItemView> {
                    items
                        .iter()
                        .map(|it| {
                            let (id, exact) = ai::resolve_path(&t, &it.path);
                            let node = (exact && t.is_alive(id) && id != t.root && !matches!(t.get(id).kind, crate::tree::Kind::Hidden | crate::tree::Kind::Mount)).then_some(id);
                            let size = node.map(|n| t.get(n).size).unwrap_or(0);
                            let collected = node.is_some_and(|n| s.is_collected(&t, n));
                            PanelItemView {
                                path: it.path.clone(),
                                reason: if it.reason.is_empty() { it.guess.clone() } else { it.reason.clone() },
                                node,
                                size,
                                collected,
                            }
                        })
                        .collect()
                };
                let tri = s.ai.triage.as_ref().map(|t2| (mk(&t2.safe), mk(&t2.verify), mk(&t2.your_call), t2.summary.clone(), t2.reclaimable.clone(), t2.commands.clone()));
                (s.ai.teaser.clone(), tri, s.ai.history.clone(), s.ai.phase, s.ai.follow_ups.clone(), s.ai.verify_open)
            };
            let busy = phase == Phase::Busy;

            let mut p = ui.painter().clone();
            p.set_clip_rect(panel);
            let mut y = top;

            // teaser (local, free)
            if let Some(te) = &teaser {
                let txt = format!("{} · {} in {} known cache/build folders", tr("Local pass"), fmt_size(te.total), te.count);
                let g = p.layout(txt, font(11.5), theme.text_faint, body.width());
                p.galley(Pos2::new(body.left(), y), g, theme.text_faint);
                y += 20.0;
            }

            // idle: the Ask button
            if phase == Phase::Idle && !self.sessions[si].ai.analyzed {
                let b = Rect::from_min_size(Pos2::new(body.left(), y + 6.0), Vec2::new(body.width(), 40.0));
                if widgets::button_ex(ui, b, Id::new(("ai-analyze", si)), tr("Analyze this scan"), None, BtnStyle::Primary, &theme, !scanning).clicked() {
                    action = Some(0);
                }
                let host = self.settings.ai_endpoint.trim_start_matches("https://").split('/').next().unwrap_or("").to_string();
                let note = format!("{} · {} {}", self.settings.ai_model, tr("sends folder names and sizes to"), host);
                let g = p.layout(note, font(10.5), theme.text_faint, body.width());
                p.galley(Pos2::new(body.left(), y + 52.0), g, theme.text_faint);
                y += 76.0;
            }

            match phase {
                Phase::Busy => {
                    let dots = ".".repeat((self.time as usize * 2 % 3) + 1);
                    p.text(Pos2::new(body.left(), y + 8.0), Align2::LEFT_CENTER, format!("{}{}", tr("Thinking"), dots), bold(13.0), theme.accent2);
                    y += 28.0;
                }
                Phase::Error => {
                    let err = self.sessions[si].ai.error.clone().unwrap_or_default();
                    let g = p.layout(format!("{} {err}", tr("Request failed:")), font(12.0), theme.danger, body.width());
                    let gh = g.size().y;
                    p.galley(Pos2::new(body.left(), y), g, theme.danger);
                    y += gh + 6.0;
                    let b = Rect::from_min_size(Pos2::new(body.left(), y), Vec2::new(110.0, 30.0));
                    if widgets::button(ui, b, Id::new(("ai-retry", si)), tr("Retry"), Some(Icon::Refresh), BtnStyle::Subtle, &theme).clicked() {
                        action = Some(2);
                    }
                    y += 38.0;
                }
                _ => {}
            }

            // triage
            if let Some((safe, verify, your_call, summary, reclaimable, commands)) = triage_view {
                if !summary.is_empty() {
                    let g = p.layout(summary, font(12.5), theme.text, body.width());
                    let gh = g.size().y;
                    p.galley(Pos2::new(body.left(), y), g, theme.text);
                    y += gh + 8.0;
                }
                if !reclaimable.is_empty() {
                    p.text(Pos2::new(body.left(), y), Align2::LEFT_CENTER, format!("{} {reclaimable}", tr("Reclaimable:")), bold(13.0), theme.ok);
                    y += 22.0;
                }
                if !safe.is_empty() {
                    let safe_bytes: u64 = safe.iter().map(|i| i.size).sum();
                    let (h, a) = self.ai_tier_header(ui, body, y, tr("SAFE"), theme.ok, safe.len(), safe_bytes, Some(tr("Collect all")), &theme, si, 0);
                    if a {
                        collect.extend(safe.iter().filter_map(|i| i.node));
                    }
                    y += h;
                    for it in safe.iter() {
                        y += self.ai_item_row(ui, body, y, it, theme.ok, &theme, si, 's', &mut collect);
                    }
                }
                if !verify.is_empty() {
                    let verify_bytes: u64 = verify.iter().map(|i| i.size).sum();
                    let label = if verify_open { tr("Hide") } else { tr("Review") };
                    let (h, a) = self.ai_tier_header(ui, body, y, tr("VERIFY"), theme.warn, verify.len(), verify_bytes, Some(label), &theme, si, 1);
                    if a {
                        action = Some(5);
                    }
                    y += h;
                    if verify_open {
                        for it in verify.iter() {
                            y += self.ai_item_row(ui, body, y, it, theme.warn, &theme, si, 'v', &mut collect);
                        }
                    }
                }
                if !your_call.is_empty() {
                    let yc_bytes: u64 = your_call.iter().map(|i| i.size).sum();
                    let (h, _) = self.ai_tier_header(ui, body, y, tr("YOUR CALL"), theme.text_dim, your_call.len(), yc_bytes, None, &theme, si, 2);
                    y += h;
                    for it in your_call.iter() {
                        y += self.ai_item_row(ui, body, y, it, theme.text_dim, &theme, si, 'y', &mut collect);
                    }
                }
                if !commands.is_empty() {
                    p.text(Pos2::new(body.left(), y + 2.0), Align2::LEFT_CENTER, tr("SUGGESTED COMMANDS"), bold(10.5), theme.text_faint);
                    y += 20.0;
                    for (ci, c) in commands.iter().enumerate() {
                        let mono = FontId::monospace(11.0);
                        let w = (body.width() - 74.0).max(60.0);
                        let line = widgets::truncate(&p, &c.command, &mono, w);
                        p.text(Pos2::new(body.left() + 6.0, y + 8.0), Align2::LEFT_CENTER, line, mono, theme.text);
                        let cb = Rect::from_min_size(Pos2::new(body.right() - 66.0, y), Vec2::new(66.0, 26.0));
                        if widgets::button(ui, cb, Id::new(("ai-cmd", si, ci)), tr("Copy"), None, BtnStyle::Subtle, &theme).clicked() {
                            copy_cmd = Some(c.command.clone());
                        }
                        let why = widgets::truncate(&p, &c.why, &font(10.5), body.width() - 12.0);
                        p.text(Pos2::new(body.left() + 6.0, y + 24.0), Align2::LEFT_CENTER, why, font(10.5), theme.text_faint);
                        y += 40.0;
                    }
                }
                y += 8.0;
            }

            // follow-up expansions the model asked for
            if !follow_ups.is_empty() && !busy {
                let g = p.layout(tr("The AI wants to look inside:").to_string(), bold(11.5), theme.text_dim, body.width());
                let gh = g.size().y;
                p.galley(Pos2::new(body.left(), y), g, theme.text_dim);
                y += gh + 4.0;
                for path in follow_ups.iter() {
                    let mono = FontId::monospace(10.5);
                    let line = widgets::truncate(&p, path, &mono, body.width() - 8.0);
                    p.text(Pos2::new(body.left() + 8.0, y), Align2::LEFT_CENTER, line, mono, theme.accent2);
                    y += 16.0;
                }
                let b = Rect::from_min_size(Pos2::new(body.left(), y + 4.0), Vec2::new(160.0, 28.0));
                if widgets::button(ui, b, Id::new(("ai-expand", si)), tr("Send expansions"), Some(Icon::Eye), BtnStyle::Subtle, &theme).clicked() {
                    action = Some(4);
                }
                y += 38.0;
            }

            // chat history (skip [0]=dossier; [1] is rendered above when it parsed as triage)
            let skip_first_reply = self.sessions[si].ai.triage.is_some();
            let start = 1 + skip_first_reply as usize;
            for m in history.iter().skip(start) {
                if m.role == "user" {
                    let g = p.layout(format!("{} {}", tr("you:"), m.content), font(12.0), theme.accent2, body.width() - 8.0);
                    let gh = g.size().y;
                    p.galley(Pos2::new(body.left() + 8.0, y + 6.0), g, theme.accent2);
                    y += gh + 12.0;
                } else {
                    let g = p.layout(m.content.clone(), font(12.0), theme.text, body.width());
                    let h = g.size().y + 10.0;
                    p.rect_filled(Rect::from_min_size(Pos2::new(body.left(), y), Vec2::new(2.0, h)), cr(1.0), with_alpha(theme.accent, 0.5));
                    p.galley(Pos2::new(body.left() + 10.0, y), g, theme.text);
                    y += h;
                }
            }
            y += 4.0;
            self.sessions[si].ai.content_h = y - body.top() + 8.0;

            // input row
            let can_send = !busy && !self.sessions[si].ai.input.trim().is_empty();
            let resp = ui.put(
                input,
                egui::TextEdit::singleline(&mut self.sessions[si].ai.input)
                    .hint_text(tr("Ask about this scan…").to_owned())
                    .desired_width(input.width()),
            );
            if widgets::button_ex(ui, send_r, Id::new(("ai-send", si)), "→", Some(Icon::Forward), BtnStyle::Primary, &theme, can_send).clicked() {
                action = Some(1);
            }
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && can_send {
                action = Some(1);
            }
        }

        // apply actions
        match action {
            Some(0) => self.ai_analyze(si, &ctx),
            Some(1) => self.ai_send_chat(si, &ctx),
            Some(2) => self.ai_call(si, ai::chat_system_prompt(), 0.3, &ctx),
            Some(3) => self.open_modal(Modal::Settings),
            Some(4) => self.ai_send_expansions(si, &ctx),
            Some(5) => self.sessions[si].ai.verify_open = !self.sessions[si].ai.verify_open,
            _ => {}
        }
        for node in collect {
            if self.sessions[si].collect(node) {
                self.sfx(crate::sound::Sfx::Plop);
                self.bin_bounce = 1.0;
            }
        }
        if let Some(c) = copy_cmd {
            ctx.copy_text(c);
            self.toasts.push(tr("Copied").into(), self.theme.accent);
        }
    }

    /// Tier header row; returns its height and whether its button was clicked.
    fn ai_tier_header(
        &mut self,
        ui: &mut egui::Ui,
        body: Rect,
        y: f32,
        label: &str,
        color: Color32,
        n: usize,
        bytes: u64,
        btn: Option<&str>,
        theme: &Theme,
        si: usize,
        tier: u8,
    ) -> (f32, bool) {
        let p = ui.painter().clone();
        p.text(Pos2::new(body.left(), y + 8.0), Align2::LEFT_CENTER, label, bold(11.0), color);
        let info = format!("{n} · {}", fmt_size(bytes));
        let mut clicked = false;
        let bw = btn.map(|b| p.layout_no_wrap(b.to_string(), bold(11.0), Color32::WHITE).size().x + 24.0);
        p.text(
            Pos2::new(body.right() - bw.unwrap_or(0.0) - 4.0, y + 8.0),
            Align2::RIGHT_CENTER,
            info,
            font(11.0),
            theme.text_dim,
        );
        if let (Some(b), Some(w)) = (btn, bw) {
            let r = Rect::from_min_size(Pos2::new(body.right() - w, y), Vec2::new(w, 22.0));
            if widgets::button(ui, r, Id::new(("ai-tier", si, tier)), b, None, BtnStyle::Subtle, theme).clicked() {
                clicked = true;
            }
        }
        p.line_segment([Pos2::new(body.left(), y + 21.0), Pos2::new(body.right(), y + 21.0)], Stroke::new(1.0, with_alpha(theme.stroke, 0.5)));
        (28.0, clicked)
    }

    /// One suggestion row; returns its height. Collect clicks go into `collect`.
    fn ai_item_row(
        &mut self,
        ui: &mut egui::Ui,
        body: Rect,
        y: f32,
        it: &PanelItemView,
        color: Color32,
        theme: &Theme,
        si: usize,
        tier: char,
        collect: &mut Vec<u32>,
    ) -> f32 {
        let p = ui.painter().clone();
        let valid = it.node.is_some();
        let w = body.width();
        let mono = FontId::monospace(11.0);
        // tier tick
        p.rect_filled(Rect::from_min_size(Pos2::new(body.left(), y + 2.0), Vec2::new(2.0, 30.0)), cr(1.0), with_alpha(color, 0.7));
        if valid {
            let path_line = widgets::truncate(&p, &it.path, &mono, w - 96.0);
            p.text(Pos2::new(body.left() + 8.0, y + 8.0), Align2::LEFT_CENTER, path_line, mono, theme.text);
            p.text(Pos2::new(body.right() - 36.0, y + 8.0), Align2::RIGHT_CENTER, fmt_size(it.size), bold(11.0), theme.text_dim);
            if it.collected {
                widgets::draw_icon(&p, Icon::Check, Rect::from_center_size(Pos2::new(body.right() - 20.0, y + 8.0), Vec2::splat(13.0)), theme.ok);
            } else {
                let b = Rect::from_center_size(Pos2::new(body.right() - 20.0, y + 8.0), Vec2::splat(22.0));
                if widgets::icon_button(ui, b, Id::new(("ai-chip", si, tier, it.path.as_str())), Icon::Plus, theme, true).clicked() {
                    if let Some(node) = it.node {
                        collect.push(node);
                    }
                }
            }
        } else {
            let path_line = widgets::truncate(&p, &it.path, &mono, w - 84.0);
            p.text(Pos2::new(body.left() + 8.0, y + 8.0), Align2::LEFT_CENTER, path_line, mono, theme.text_faint);
            if tier != 'y' {
                p.text(Pos2::new(body.right() - 4.0, y + 8.0), Align2::RIGHT_CENTER, tr("not in scan"), font(9.5), theme.warn);
            }
        }
        let reason = widgets::truncate(&p, &it.reason, &font(10.5), w - 24.0);
        p.text(Pos2::new(body.left() + 14.0, y + 24.0), Align2::LEFT_CENTER, reason, font(10.5), theme.text_faint);
        40.0
    }
}
