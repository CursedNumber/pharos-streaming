// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 The Pharos authors

//! Universal-design helpers.
//!
//! **Project directive:** every meaningful, interactive, or non-obvious widget in
//! the panel gets a generous human description that is BOTH shown on hover (for
//! sighted users) AND announced by screen readers. egui already exposes a
//! widget's own text/label/value to AccessKit; these helpers add the
//! supplementary *description* (announced after the name) and, where a control
//! has no inherent text of its own (icon-only buttons, painted canvas regions),
//! an explicit accessible *name* too.
//!
//! AccessKit is enabled via eframe's `accesskit` feature; on Linux it bridges to
//! AT-SPI (Orca), on Windows to UI Automation, on macOS to NSAccessibility.
//! [`egui::Context::accesskit_node_builder`] is a no-op (returns `None`) unless a
//! screen reader is actually attached, so calling `describe` on every widget
//! every frame is cheap.

use egui::Response;

/// Attach screen-reader + hover descriptions to a [`Response`].
///
/// Use `.describe("…")` on every control and every non-obvious display element.
/// Reach for `.describe_named(name, desc)` when the widget carries no readable
/// text of its own (icon-only buttons, painted tiles), so a screen reader has a
/// name to announce.
pub trait Describe {
    /// Set the widget's AccessKit description (screen-reader) and a matching
    /// hover tooltip (sighted). The widget keeps its own accessible name.
    fn describe(self, text: impl Into<String>) -> Self;

    /// Like [`describe`](Describe::describe) but also sets the accessible *name*.
    /// For icon-only / painted widgets whose visible content doesn't say what
    /// they are: screen readers announce `name`, then `desc`.
    fn describe_named(self, name: impl Into<String>, desc: impl Into<String>) -> Self;
}

impl Describe for Response {
    fn describe(self, text: impl Into<String>) -> Self {
        let text = text.into();
        self.ctx.accesskit_node_builder(self.id, |b| {
            b.set_description(text.as_str());
        });
        self.on_hover_text(text)
    }

    fn describe_named(self, name: impl Into<String>, desc: impl Into<String>) -> Self {
        let name = name.into();
        let desc = desc.into();
        self.ctx.accesskit_node_builder(self.id, |b| {
            b.set_name(name.as_str());
            b.set_description(desc.as_str());
        });
        self.on_hover_text(desc)
    }
}
