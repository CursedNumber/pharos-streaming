// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Small reusable widgets shared across tabs, matching the client's card +
//! badge idioms.

use std::borrow::Cow;

use crate::model::Liveness;
use crate::theme;

/// Card-render-layer truncator: long controller (or other) names are clipped to
/// 32 characters so a runaway "Connected to OneDrive – Personal – Workshop Mac"
/// alias can't blow out the card layout. Splits on **character** boundaries
/// (not bytes) so non-ASCII names don't panic. Storage keeps the full string —
/// editing renames the raw alias, this helper only shapes the on-screen text.
pub fn truncate_for_card(s: &str) -> Cow<'_, str> {
    const MAX: usize = 32;
    if s.chars().count() <= MAX {
        Cow::Borrowed(s)
    } else {
        // 31 chars + horizontal-ellipsis (… = 1 visible char) → 32 visible chars.
        let head: String = s.chars().take(MAX - 1).collect();
        Cow::Owned(format!("{head}\u{2026}"))
    }
}

/// A status badge. Pass a [`Liveness`] (colour + label derived from it) or an
/// explicit `(text, Color32)` via [`BadgeKind`].
pub enum BadgeKind<'a> {
    Live(Liveness),
    Custom(&'a str, egui::Color32),
}

impl From<Liveness> for BadgeKind<'_> {
    fn from(l: Liveness) -> Self {
        BadgeKind::Live(l)
    }
}

impl<'a> From<(&'a str, egui::Color32)> for BadgeKind<'a> {
    fn from((t, c): (&'a str, egui::Color32)) -> Self {
        BadgeKind::Custom(t, c)
    }
}

/// Draw a status badge from either a [`Liveness`] or a `(text, Color32)` pair:
/// `status_badge(ui, Liveness::Online)` or `status_badge(ui, ("Paired", GREEN))`.
/// Returns the painted [`egui::Response`] so a caller can attach an a11y
/// description to the badge (existing call sites may ignore it).
pub fn status_badge<'a>(ui: &mut egui::Ui, kind: impl Into<BadgeKind<'a>>) -> egui::Response {
    match kind.into() {
        BadgeKind::Live(l) => theme::badge(ui, &l.label(), l.color()),
        BadgeKind::Custom(t, c) => theme::badge(ui, t, c),
    }
}

/// A `Frame::group` card matching the client's tiles: rounding 10, inner margin
/// 12, filled with `inactive.weak_bg_fill` so it reads on either theme AND
/// contrasts with the slider rail (which egui paints in `inactive.bg_fill`).
/// `id` namespaces the inner layout (use a unique string per card).
pub fn card<R>(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash,
    content: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let frame = egui::Frame::group(ui.style())
        .rounding(egui::Rounding::same(10.0))
        .inner_margin(egui::Margin::same(12.0))
        .fill(ui.visuals().widgets.inactive.weak_bg_fill);
    frame
        .show(ui, |ui| ui.push_id(id, |ui| content(ui)).inner)
        .inner
}
