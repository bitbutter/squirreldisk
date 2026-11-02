//! The AI tab of the side panel (SquirrelDisk-AI fork).
//!
//! Trust model: the model's suggestions are data. Paths are validated against
//! the live tree; valid ones become collector candidates through the app's
//! existing `Session::collect`, so every deletion still passes the app's
//! safety checks and its confirm dialog (DECISIONS.md, DEC-AI-002). Nothing
//! the model says is ever executed; suggested commands are copy-only.
//! Clickable paths navigate the sunburst to the item (DEC-AI-008) — the same
//! zoom a click on the chart performs; it only ever changes the view.

use super::app::{App, Modal, Target};
use super::sunburst::View;
use super::theme::{with_alpha, Theme};
use super::widgets::{self, bold, cr, font, BtnStyle, Icon};
use crate::ai::{self, Msg, Phase};
use crate::i18n::tr;
use crate::tree::{fmt_size, Kind};
use eframe::egui::{self, Align, Align2, Color32, FontId, Id, Pos2, Rect, Sense, Stroke, Vec2};

/// Panel background + border, shared by the file list and the AI tab.
pub fn frame(ui: &egui::Ui, panel: Rect, theme: &Theme) {
    let p = ui.painter();
    widgets::shadow(p, panel, 18.0, 0.5, theme);
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
    let mut next_x = strip.left() + 18.0;
    for (k, (label, is_ai)) in [("FILES", false), ("AI", true)].into_iter().enumerate() {
        let active = is_ai == app.sessions[si].ai.open;
        let g = p.layout_no_wrap(label.to_string(), bold(10.5), Color32::WHITE);
        let w = g.size().x + 18.0;
        let r = Rect::from_min_size(Pos2::new(next_x, strip.center().y - 9.0), Vec2::new(w, 18.0));
        next_x = r.right() + 6.0;
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
    // unread dot: scan finished, the AI tab has news
    {
        let s = &mut app.sessions[si];
        if !s.ai.open && s.ai.teaser.is_some() && !s.ai.teaser_seen {
            p.circle_filled(Pos2::new(next_x - 6.0, strip.center().y), 3.5, theme.warn);
        }
        match clicked {
            Some(true) => {
                s.ai.open = true;
                s.ai.teaser_seen = true;
                s.ai.manual_files = false;
            }
            Some(false) => {
                s.ai.open = false;
                s.ai.manual_files = true; // deliberate switch: stop auto-opening
            }
            None => {}
        }
    }
}

/// (resolved bytes, unresolved count) for a tier's header line.
fn tier_stats(items: &[PanelItemView]) -> (u64, usize) {
    items.iter().fold((0u64, 0usize), |(b, u), i| if i.node.is_some() { (b + i.size, u) } else { (b, u + 1) })
}

/// One suggestion row, resolved against the live tree.
pub struct PanelItemView {
    pub path: String,
    pub reason: String,
    pub node: Option<u32>,
    pub size: u64,
    pub collected: bool,
}

/// Actions collected while drawing, applied once the tree lock is gone.
#[derive(Default)]
struct Actions {
    collect: Vec<u32>,
    navigate: Option<u32>,
    copy_cmd: Option<String>,
    /// 0 analyze, 1 send, 2 retry, 3 open settings, 4 expand, 5 toggle verify
    action: Option<u8>,
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
        let s = &mut self.sessions[si];
        s.ai.history.push(ai::user(text));
        s.ai.jump = true;
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
        let s = &mut self.sessions[si];
        s.ai.history.push(ai::user(text));
        s.ai.jump = true;
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
                            // the raw reply is always kept: it is the model's
                            // turn in the conversation and the prose fallback
                            // when the JSON did not parse (DEC-AI-007)
                            s.ai.history.push(ai::assistant(reply));
                            // the triage answer reads from the top: no jump
                        } else {
                            let (text, expand) = ai::take_expand_request(&reply);
                            if let Some(p) = expand {
                                s.ai.follow_ups.push(p);
                            }
                            s.ai.history.push(ai::assistant(text));
                            s.ai.jump = true;
                        }
                        s.ai.phase = Phase::Done;
                        s.ai.refocus = true;
                    }
                    Err(e) => {
                        s.ai.error = Some(e);
                        s.ai.phase = Phase::Error;
                    }
                }
            }
        }

        let mut a = Actions::default();

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
                a.action = Some(3);
            }
        } else {
            let body = Rect::from_min_max(Pos2::new(panel.left() + 18.0, panel.top() + 16.0 + 26.0), Pos2::new(panel.right() - 18.0, panel.bottom() - 16.0 - 44.0));
            let input = Rect::from_min_size(Pos2::new(body.left(), panel.bottom() - 16.0 - 32.0), Vec2::new(body.width() - 48.0, 32.0));
            let send_r = Rect::from_min_size(Pos2::new(input.right() + 8.0, input.top()), Vec2::new(40.0, 32.0));

            // gather view data under one tree lock
            let (teaser, triage_view, history, phase, follow_ups, verify_open) = {
                let s = &self.sessions[si];
                let t = s.tree.read().unwrap();
                let mk = |items: &[ai::Item]| -> Vec<PanelItemView> {
                    items
                        .iter()
                        .map(|it| {
                            let (id, exact) = ai::resolve_path(&t, &it.path);
                            let node = (exact && t.is_alive(id) && id != t.root && !matches!(t.get(id).kind, Kind::Hidden | Kind::Mount)).then_some(id);
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

            // A real ScrollArea: wheel, touch, scrollbar, correct clamping and
            // interaction clipping (off-screen rows can no longer be clicked).
            ui.scope_builder(egui::UiBuilder::new().max_rect(body), |su| {
                egui::ScrollArea::vertical()
                    .id_salt(Id::new(("ai-scroll", si)))
                    .auto_shrink([false, false])
                    .show(su, |inner| {
                        let p = inner.painter().clone();
                        let left = inner.cursor().left();
                        let w = inner.available_width().max(1.0);
                        let right = left + w;
                        let y0 = inner.cursor().top();
                        let mut y = y0;

                        // teaser (local, free)
                        if let Some(te) = &teaser {
                            let txt = format!("{} · {} in {} known cache/build folders", tr("Local pass"), fmt_size(te.total), te.count);
                            let g = p.layout(txt, font(11.5), theme.text_faint, w);
                            let gh = g.size().y;
                            p.galley(Pos2::new(left, y), g, theme.text_faint);
                            y += gh + 4.0;
                        }

                        // idle: the Ask button
                        if phase == Phase::Idle && !self.sessions[si].ai.analyzed {
                            let b = Rect::from_min_size(Pos2::new(left, y + 6.0), Vec2::new(w, 40.0));
                            if widgets::button_ex(inner, b, Id::new(("ai-analyze", si)), tr("Analyze this scan"), None, BtnStyle::Primary, &theme, !scanning).clicked() {
                                a.action = Some(0);
                            }
                            let host = self.settings.ai_endpoint.split("://").nth(1).unwrap_or(&self.settings.ai_endpoint).split('/').next().unwrap_or("").to_string();
                            let note = format!("{} · {} {}", self.settings.ai_model, tr("sends folder names and sizes to"), host);
                            let g = p.layout(note, font(10.5), theme.text_faint, w);
                            let gh = g.size().y;
                            p.galley(Pos2::new(left, y + 52.0), g, theme.text_faint);
                            y += 52.0 + gh + 8.0;
                        }

                        match phase {
                            Phase::Busy => {
                                let dots = ".".repeat((self.time as usize * 2 % 3) + 1);
                                p.text(Pos2::new(left, y + 8.0), Align2::LEFT_CENTER, format!("{}{}", tr("Thinking"), dots), bold(13.0), theme.accent2);
                                y += 28.0;
                            }
                            Phase::Error => {
                                let err = self.sessions[si].ai.error.clone().unwrap_or_default();
                                let g = p.layout(format!("{} {err}", tr("Request failed:")), font(12.0), theme.danger, w);
                                let gh = g.size().y;
                                p.galley(Pos2::new(left, y), g, theme.danger);
                                y += gh + 6.0;
                                let b = Rect::from_min_size(Pos2::new(left, y), Vec2::new(110.0, 30.0));
                                if widgets::button(inner, b, Id::new(("ai-retry", si)), tr("Retry"), Some(Icon::Refresh), BtnStyle::Subtle, &theme).clicked() {
                                    a.action = Some(2);
                                }
                                y += 38.0;
                            }
                            _ => {}
                        }

                        // triage
                        if let Some((safe, verify, your_call, summary, reclaimable, commands)) = triage_view {
                            if !summary.is_empty() {
                                let g = p.layout(summary, font(12.5), theme.text, w);
                                let gh = g.size().y;
                                p.galley(Pos2::new(left, y), g, theme.text);
                                y += gh + 8.0;
                            }
                            if !reclaimable.is_empty() {
                                p.text(Pos2::new(left, y), Align2::LEFT_CENTER, format!("{} {reclaimable}", tr("Reclaimable:")), bold(13.0), theme.ok);
                                y += 22.0;
                            }
                            if !safe.is_empty() {
                                let (safe_bytes, unresolved) = tier_stats(&safe);
                                let label = if unresolved > 0 { format!("{} · {} not in scan", tr("Collect all"), unresolved) } else { tr("Collect all").to_string() };
                                let (h, clicked) = self.ai_tier_header(inner, left, right, y, tr("SAFE"), theme.ok, safe.len(), safe_bytes, Some(&label), &theme, si, 0);
                                if clicked {
                                    a.collect.extend(safe.iter().filter_map(|i| i.node));
                                }
                                y += h;
                                for it in safe.iter() {
                                    y += self.ai_item_row(inner, left, right, y, w, it, theme.ok, &theme, si, 's', &mut a);
                                }
                            }
                            if !verify.is_empty() {
                                let (verify_bytes, _unresolved) = tier_stats(&verify);
                                let label = if verify_open { tr("Hide").to_string() } else { tr("Review").to_string() };
                                let (h, clicked) = self.ai_tier_header(inner, left, right, y, tr("VERIFY"), theme.warn, verify.len(), verify_bytes, Some(&label), &theme, si, 1);
                                if clicked {
                                    a.action = Some(5);
                                }
                                y += h;
                                if verify_open {
                                    for it in verify.iter() {
                                        y += self.ai_item_row(inner, left, right, y, w, it, theme.warn, &theme, si, 'v', &mut a);
                                    }
                                }
                            }
                            if !your_call.is_empty() {
                                let yc_bytes: u64 = your_call.iter().map(|i| i.size).sum();
                                let (h, _) = self.ai_tier_header(inner, left, right, y, tr("YOUR CALL"), theme.text_dim, your_call.len(), yc_bytes, None, &theme, si, 2);
                                y += h;
                                for it in your_call.iter() {
                                    y += self.ai_item_row(inner, left, right, y, w, it, theme.text_dim, &theme, si, 'y', &mut a);
                                }
                            }
                            if !commands.is_empty() {
                                p.text(Pos2::new(left, y + 2.0), Align2::LEFT_CENTER, tr("SUGGESTED COMMANDS"), bold(10.5), theme.text_faint);
                                y += 20.0;
                                for (ci, c) in commands.iter().enumerate() {
                                    let mono = FontId::monospace(11.0);
                                    let cw = (w - 74.0).max(60.0);
                                    let line = widgets::truncate(&p, &c.command, &mono, cw);
                                    p.text(Pos2::new(left + 6.0, y + 8.0), Align2::LEFT_CENTER, line, mono, theme.text);
                                    let cb = Rect::from_min_size(Pos2::new(right - 66.0, y), Vec2::new(66.0, 26.0));
                                    if widgets::button(inner, cb, Id::new(("ai-cmd", si, ci)), tr("Copy"), None, BtnStyle::Subtle, &theme).clicked() {
                                        a.copy_cmd = Some(c.command.clone());
                                    }
                                    let why = widgets::truncate(&p, &c.why, &font(10.5), w - 12.0);
                                    p.text(Pos2::new(left + 6.0, y + 24.0), Align2::LEFT_CENTER, why, font(10.5), theme.text_faint);
                                    y += 40.0;
                                }
                            }
                            y += 8.0;
                        }

                        // follow-up expansions the model asked for
                        if !follow_ups.is_empty() && !busy {
                            let g = p.layout(tr("The AI wants to look inside:").to_string(), bold(11.5), theme.text_dim, w);
                            let gh = g.size().y;
                            p.galley(Pos2::new(left, y), g, theme.text_dim);
                            y += gh + 4.0;
                            for path in follow_ups.iter() {
                                let mono = FontId::monospace(10.5);
                                let line = widgets::truncate(&p, path, &mono, w - 8.0);
                                let lh = 16.0;
                                let rr = Rect::from_min_size(Pos2::new(left + 4.0, y), Vec2::new(w - 8.0, lh));
                                let node = {
                                    let t = self.sessions[si].tree.read().unwrap();
                                    let (id, exact) = ai::resolve_path(&t, path);
                                    (exact && t.is_alive(id) && id != t.root).then_some(id)
                                };
                                let resp = inner.interact(rr, Id::new(("ai-fu", si, path.as_str())), Sense::click());
                                if resp.hovered() && node.is_some() {
                                    p.rect_filled(rr, cr(4.0), with_alpha(theme.accent, 0.12));
                                    inner.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                }
                                p.text(Pos2::new(left + 8.0, y), Align2::LEFT_CENTER, line, mono, if node.is_some() { theme.accent2 } else { theme.text_faint });
                                if resp.clicked() {
                                    if let Some(n) = node {
                                        a.navigate = Some(n);
                                    }
                                }
                                y += lh;
                            }
                            let b = Rect::from_min_size(Pos2::new(left, y + 4.0), Vec2::new(160.0, 28.0));
                            if widgets::button(inner, b, Id::new(("ai-expand", si)), tr("Send expansions"), Some(Icon::Eye), BtnStyle::Subtle, &theme).clicked() {
                                a.action = Some(4);
                            }
                            y += 38.0;
                        }

                        // chat history (skip [0]=dossier; [1] is rendered above when it parsed as triage)
                        let skip_first_reply = self.sessions[si].ai.triage.is_some();
                        let start = 1 + skip_first_reply as usize;
                        for (mi, m) in history.iter().enumerate().skip(start) {
                            if m.role == "user" {
                                let g = p.layout(format!("{} {}", tr("you:"), m.content), font(12.0), theme.accent2, w - 8.0);
                                let gh = g.size().y;
                                p.galley(Pos2::new(left + 8.0, y + 6.0), g, theme.accent2);
                                y += gh + 12.0;
                            } else {
                                // assistant text, line by line: a line that
                                // names a real path is clickable and zooms
                                // the sunburst to it (DEC-AI-008)
                                for (li, line) in m.content.lines().enumerate() {
                                    let g = p.layout(line.to_string(), font(12.0), theme.text, w - 12.0);
                                    let gh = g.size().y.max(14.0);
                                    let rr = Rect::from_min_size(Pos2::new(left + 2.0, y), Vec2::new(w - 4.0, gh));
                                    let node = self.ai_cached_path(si, mi, li, line);
                                    let id = Id::new(("ai-line", si, mi, li));
                                    let resp = inner.interact(rr, id, if node.is_some() { Sense::click() } else { Sense::hover() });
                                    let hov = resp.hovered() && node.is_some();
                                    if hov {
                                        p.rect_filled(rr, cr(4.0), with_alpha(theme.accent, 0.10));
                                        inner.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                    }
                                    p.galley(Pos2::new(left + 10.0, y), g, theme.text);
                                    if resp.clicked() {
                                        if let Some(n) = node {
                                            a.navigate = Some(n);
                                        }
                                    }
                                    y += gh + 2.0;
                                }
                                y += 8.0;
                            }
                        }

                        // tell the ScrollArea how tall the content is
                        let total = (y - y0 + 4.0).max(4.0);
                        let end = inner.allocate_exact_size(Vec2::new(w, total), Sense::hover());
                        if self.sessions[si].ai.jump {
                            end.1.scroll_to_me(Some(Align::Max));
                            self.sessions[si].ai.jump = false;
                        }
                    });
            });

            // input row (pinned below the scroll area); chat needs the
            // dossier in context — gate on a completed Analyze
            let can_send = !busy && self.sessions[si].ai.analyzed && !self.sessions[si].ai.input.trim().is_empty();
            let want_focus = self.sessions[si].ai.refocus && !busy;
            let analyzed = self.sessions[si].ai.analyzed;
            let resp = ui.put(
                input,
                egui::TextEdit::singleline(&mut self.sessions[si].ai.input)
                    .hint_text(if analyzed { tr("Ask about this scan…").to_owned() } else { tr("Analyze first, then ask…").to_owned() })
                    .desired_width(input.width()),
            );
            if widgets::button_ex(ui, send_r, Id::new(("ai-send", si)), "→", Some(Icon::Forward), BtnStyle::Primary, &theme, can_send).clicked() {
                a.action = Some(1);
            }
            if want_focus {
                resp.request_focus();
                self.sessions[si].ai.refocus = false;
            }
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && can_send {
                a.action = Some(1);
                self.sessions[si].ai.refocus = true;
            }
        }

        // apply actions (tree lock and borrows are gone)
        match a.action {
            Some(0) => self.ai_analyze(si, &ctx),
            Some(1) => self.ai_send_chat(si, &ctx),
            Some(2) => {
                // retry the call that failed: analyze while no first reply
                // has landed, chat afterwards
                let system = if self.sessions[si].ai.analyzed { ai::chat_system_prompt() } else { ai::analyze_system_prompt() };
                self.ai_call(si, system, 0.3, &ctx);
            }
            Some(3) => self.open_modal(Modal::Settings),
            Some(4) => self.ai_send_expansions(si, &ctx),
            Some(5) => self.sessions[si].ai.verify_open = !self.sessions[si].ai.verify_open,
            _ => {}
        }
        for node in a.collect {
            if self.sessions[si].collect(node) {
                self.sfx(crate::sound::Sfx::Plop);
                self.bin_bounce = 1.0;
            }
        }
        if let Some(n) = a.navigate.filter(|&n| self.sessions[si].tree.read().unwrap().is_alive(n)) {
            self.sessions[si].navigate(View::node(n));
            self.sfx(crate::sound::Sfx::Blip);
        }
        if let Some(c) = a.copy_cmd {
            ctx.copy_text(c);
            self.toasts.push(tr("Copied").into(), self.theme.accent);
        }
    }

    /// Cached `ai_path_in_line`: history is append-only, so (message, line)
    /// indices are stable and each line resolves against the tree exactly once.
    fn ai_cached_path(&mut self, si: usize, mi: usize, li: usize, line: &str) -> Option<u32> {
        let key = (mi, li);
        if let Some(hit) = self.sessions[si].ai.link_cache.get(&key) {
            return *hit;
        }
        let node = self.ai_path_in_line(si, line).map(|(_, n)| n);
        self.sessions[si].ai.link_cache.insert(key, node);
        node
    }

    /// If `line` names exactly one path that exists in the scan, return it
    /// with its node. Cheap: token filter first, tree walk only for suspects.
    fn ai_path_in_line(&self, si: usize, line: &str) -> Option<(String, u32)> {
        if !line.contains('/') && !line.contains('\\') && !line.contains('.') {
            return None;
        }
        let t = self.sessions[si].tree.read().unwrap();
        let mut best: Option<(String, u32)> = None;
        for tok in line.split(|c: char| c == ' ' || c == '`' || c == '"' || c == ',' || c == '(' || c == ')' || c == '[' || c == ']') {
            let tok = tok.trim().trim_end_matches('.').trim_matches(':');
            if tok.len() < 3 || !tok.contains('/') && !tok.contains('\\') && !tok.contains('.') {
                continue;
            }
            let (id, exact) = ai::resolve_path(&t, tok);
            if exact && t.is_alive(id) && id != t.root && t.can_enter(id) {
                let longer = best.as_ref().map(|(b, _)| b.len() < tok.len()).unwrap_or(true);
                if longer {
                    best = Some((tok.to_string(), id));
                }
            }
        }
        best
    }

    /// Tier header row; returns its height and whether its button was clicked.
    fn ai_tier_header(
        &mut self,
        ui: &mut egui::Ui,
        left: f32,
        right: f32,
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
        p.text(Pos2::new(left, y + 8.0), Align2::LEFT_CENTER, label, bold(11.0), color);
        let info = format!("{n} · {}", fmt_size(bytes));
        let mut clicked = false;
        let bw = btn.map(|b| p.layout_no_wrap(b.to_string(), bold(11.0), Color32::WHITE).size().x + 24.0);
        p.text(Pos2::new(right - bw.unwrap_or(0.0) - 4.0, y + 8.0), Align2::RIGHT_CENTER, info, font(11.0), theme.text_dim);
        if let (Some(b), Some(w)) = (btn, bw) {
            let r = Rect::from_min_size(Pos2::new(right - w, y), Vec2::new(w, 22.0));
            if widgets::button(ui, r, Id::new(("ai-tier", si, tier)), b, None, BtnStyle::Subtle, theme).clicked() {
                clicked = true;
            }
        }
        p.line_segment([Pos2::new(left, y + 21.0), Pos2::new(right, y + 21.0)], Stroke::new(1.0, with_alpha(theme.stroke, 0.5)));
        (28.0, clicked)
    }

    /// One suggestion row; returns its height. The path text itself zooms the
    /// sunburst to the item (DEC-AI-008); the [+] stages it into the collector.
    fn ai_item_row(
        &mut self,
        ui: &mut egui::Ui,
        left: f32,
        right: f32,
        y: f32,
        w: f32,
        it: &PanelItemView,
        color: Color32,
        theme: &Theme,
        si: usize,
        tier: char,
        a: &mut Actions,
    ) -> f32 {
        let p = ui.painter().clone();
        let valid = it.node.is_some();
        let mono = FontId::monospace(11.0);
        // tier tick
        p.rect_filled(Rect::from_min_size(Pos2::new(left, y + 2.0), Vec2::new(2.0, 30.0)), cr(1.0), with_alpha(color, 0.7));
        if valid {
            let path_line = widgets::truncate(&p, &it.path, &mono, w - 96.0);
            let g = p.layout_no_wrap(path_line.clone(), mono.clone(), theme.text);
            let pw = g.size().x;
            // path text = zoom link
            let lr = Rect::from_min_size(Pos2::new(left + 8.0, y - 2.0), Vec2::new(pw + 6.0, 20.0));
            let resp = ui.interact(lr, Id::new(("ai-path", si, tier, it.path.as_str())), Sense::click());
            let hov = resp.hovered();
            if hov {
                p.rect_filled(lr, cr(4.0), with_alpha(theme.accent, 0.12));
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                p.line_segment([Pos2::new(lr.left() + 1.0, lr.bottom() - 3.0), Pos2::new(lr.left() + pw + 4.0, lr.bottom() - 3.0)], Stroke::new(1.0, theme.accent));
            }
            p.text(Pos2::new(left + 10.0, y + 8.0), Align2::LEFT_CENTER, path_line, mono, if hov { theme.accent2 } else { theme.text });
            if resp.clicked() {
                if let Some(node) = it.node {
                    a.navigate = Some(node);
                }
            }
            p.text(Pos2::new(right - 36.0, y + 8.0), Align2::RIGHT_CENTER, fmt_size(it.size), bold(11.0), theme.text_dim);
            if tier == 'y' {
                // YOUR CALL: information only — the model cannot stage what
                // only the user can judge (DECISIONS.md, DEC-AI-006)
                p.text(Pos2::new(right - 4.0, y + 8.0), Align2::RIGHT_CENTER, "?", font(11.0), theme.text_faint);
            } else if it.collected {
                widgets::draw_icon(&p, Icon::Check, Rect::from_center_size(Pos2::new(right - 20.0, y + 8.0), Vec2::splat(13.0)), theme.ok);
            } else {
                let b = Rect::from_center_size(Pos2::new(right - 20.0, y + 8.0), Vec2::splat(22.0));
                if widgets::icon_button(ui, b, Id::new(("ai-chip", si, tier, it.path.as_str())), Icon::Plus, theme, true).clicked() {
                    if let Some(node) = it.node {
                        a.collect.push(node);
                    }
                }
            }
        } else {
            let path_line = widgets::truncate(&p, &it.path, &mono, w - 84.0);
            p.text(Pos2::new(left + 8.0, y + 8.0), Align2::LEFT_CENTER, path_line, mono, theme.text_faint);
            if tier != 'y' {
                p.text(Pos2::new(right - 4.0, y + 8.0), Align2::RIGHT_CENTER, tr("not in scan"), font(9.5), theme.warn);
            }
        }
        let reason = widgets::truncate(&p, &it.reason, &font(10.5), w - 24.0);
        p.text(Pos2::new(left + 14.0, y + 24.0), Align2::LEFT_CENTER, reason, font(10.5), theme.text_faint);
        40.0
    }
}
