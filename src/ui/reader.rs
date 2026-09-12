//! Right pane: the open message.
//!
//! Header, an optional privacy notice, attachments, then the body. The body
//! is drawn by whichever backend the user selected; both receive the same
//! sanitized document, so switching backends changes only appearance.

use egui::{RichText, Ui};
use elegance::{Accent, Badge, BadgeTone, Button, ButtonSize, Callout, CalloutTone, Theme, glyphs};

use super::images::{BodyImages, RemoteImages};
use super::{Action, format_date_long, format_size};
use crate::html::Prepared;
use crate::html::native::{RenderOptions, TextureCache};
use crate::mail::{Envelope, MessageBody};

/// Everything the reader needs to draw the open message.
pub struct ReaderInput<'a> {
    pub envelope: Option<&'a Envelope>,
    pub body: Option<&'a MessageBody>,
    pub prepared: Option<&'a Prepared>,
    pub textures: &'a mut TextureCache,
    pub remote: &'a mut RemoteImages,
    /// Whether this message may load remote content.
    pub allow_remote: bool,
    pub base_size: f32,
    /// Text settings for this pane.
    pub style: crate::config::PaneStyle,
    pub show_source: &'a mut bool,
    /// The body has been requested but has not arrived.
    pub loading: bool,
    pub theme: &'a Theme,
    /// Set when the Servo backend is drawing the body, so the native
    /// renderer's scroll area is skipped.
    pub servo_drawing: bool,
}

pub fn show(ui: &mut Ui, input: ReaderInput<'_>) -> Option<Action> {
    let Some(envelope) = input.envelope else {
        return empty_state(ui, input.theme);
    };

    let mut action = None;

    header(ui, envelope, input.theme, &mut action, input.show_source);
    ui.separator();

    if let Some(prepared) = input.prepared {
        if prepared.blocked_remote > 0 && !input.allow_remote {
            let sender = envelope.from.first().map(|a| a.short());
            privacy_notice(ui, prepared.blocked_remote, sender, &mut action);
        }
    }

    if let Some(body) = input.body {
        if !body.attachments.is_empty() {
            attachments(ui, body, &mut action);
        }
    }

    if input.loading && input.body.is_none() {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.add(elegance::Spinner::new().size(18.0));
            ui.add_space(6.0);
            ui.label(input.theme.muted_text("Loading message\u{2026}"));
        });
        return action;
    }

    if *input.show_source {
        source_view(ui, input.prepared, input.body, input.base_size);
        return action;
    }

    // Servo paints into the pane itself; the caller has already drawn it.
    if input.servo_drawing {
        return action;
    }

    let Some(prepared) = input.prepared else { return action };
    let Some(body) = input.body else { return action };

    egui::ScrollArea::vertical()
        .id_salt(("body", envelope.uid))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(8.0);
            // A vertical-only scroll area clips rather than wraps, so nothing
            // in the document may exceed the viewport width.
            let width = ui.available_width();
            ui.set_max_width(width);
            let mut images = BodyImages {
                body,
                textures: input.textures,
                remote: input.remote,
                allow_remote: input.allow_remote,
            };
            let font = super::pane_font(input.style, input.base_size);
            let options = RenderOptions {
                base_size: font.size,
                family: font.family.clone(),
                max_image_width: width.max(200.0),
            };
            if let Some(url) =
                crate::html::native::show(ui, &prepared.document, &mut images, &options)
            {
                action = Some(Action::OpenUrl(url));
            }
            ui.add_space(24.0);
        });

    action
}

fn empty_state(ui: &mut Ui, theme: &Theme) -> Option<Action> {
    ui.add_space(ui.available_height() * 0.35);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(glyphs::FOLDER_OPEN.to_string()).size(36.0).color(
            ui.visuals().weak_text_color(),
        ));
        ui.add_space(10.0);
        ui.label(theme.muted_text("Select a message to read"));
    });
    None
}

fn header(
    ui: &mut Ui,
    envelope: &Envelope,
    theme: &Theme,
    action: &mut Option<Action>,
    show_source: &mut bool,
) {
    ui.add_space(6.0);

    let subject = if envelope.subject.trim().is_empty() {
        "(no subject)"
    } else {
        &envelope.subject
    };
    ui.label(RichText::new(subject).size(19.0).strong());
    ui.add_space(6.0);

    // `Sides` reserves the right-hand side first, so a long sender address
    // is truncated instead of running underneath the date.
    egui::Sides::new().shrink_left().show(
        ui,
        |ui| {
            if let Some(from) = envelope.from.first() {
                ui.label(RichText::new(from.short()).strong());
                if !from.name.is_empty() {
                    ui.add(
                        egui::Label::new(theme.muted_text(format!("<{}>", from.email)))
                            .truncate(),
                    );
                }
            }
        },
        |ui| {
            ui.label(theme.muted_text(format_date_long(envelope.date)));
        },
    );

    if !envelope.to.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label(theme.faint_text("to"));
            ui.label(theme.muted_text(join_addrs(&envelope.to)));
        });
    }
    if !envelope.cc.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label(theme.faint_text("cc"));
            ui.label(theme.muted_text(join_addrs(&envelope.cc)));
        });
    }

    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        if ui
            .add(
                Button::new(format!("{} Reply", glyphs::ARROW_LEFT))
                    .size(ButtonSize::Small)
                    .accent(Accent::Blue),
            )
            .clicked()
        {
            *action = Some(Action::Reply { all: false });
        }
        if ui
            .add(Button::new("Reply all").size(ButtonSize::Small).outline())
            .clicked()
        {
            *action = Some(Action::Reply { all: true });
        }
        if ui
            .add(
                Button::new(format!("{} Forward", glyphs::ARROW_RIGHT))
                    .size(ButtonSize::Small)
                    .outline(),
            )
            .clicked()
        {
            *action = Some(Action::Forward);
        }
        if ui
            .add(
                Button::new(format!("{} Archive", glyphs::FOLDER))
                    .size(ButtonSize::Small)
                    .outline(),
            )
            .clicked()
        {
            *action = Some(Action::Archive);
        }
        if ui
            .add(
                Button::new(format!("{} Delete", glyphs::TRASH))
                    .size(ButtonSize::Small)
                    .outline()
                    .accent(Accent::Red),
            )
            .clicked()
        {
            *action = Some(Action::Delete);
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let label = if *show_source { "Rendered" } else { "Source" };
            if ui.add(Button::new(label).size(ButtonSize::Small).outline()).clicked() {
                *show_source = !*show_source;
            }
        });
    });
    ui.add_space(6.0);
}

/// Explains what was withheld and offers to load it. Worth being explicit
/// about: loading these tells the sender the message was opened.
///
/// The two choices differ in scope, so they are separate buttons. Loading
/// this message reveals only what opening it already revealed. Trusting the
/// sender also reveals future opens, before you have decided on them.
fn privacy_notice(
    ui: &mut Ui,
    blocked: usize,
    sender: Option<&str>,
    action: &mut Option<Action>,
) {
    ui.add_space(6.0);
    Callout::new(CalloutTone::Info)
        .icon(glyphs::EYE_OFF.to_string())
        .title(format!(
            "{} remote {} blocked",
            blocked,
            if blocked == 1 { "image" } else { "images" }
        ))
        .body("Loading them tells the sender you opened this message.")
        .tinted()
        .show(ui, |ui| {
            if ui
                .add(Button::new("Load images").size(ButtonSize::Small))
                .on_hover_text("Remembered for this message")
                .clicked()
            {
                *action = Some(Action::LoadRemoteImages);
            }
            if let Some(sender) = sender {
                if ui
                    .add(Button::new("Always from sender").size(ButtonSize::Small).outline())
                    .on_hover_text(format!(
                        "Load remote content from {sender} without asking, including \
                         in messages you have not opened yet"
                    ))
                    .clicked()
                {
                    *action = Some(Action::AllowRemoteSender);
                }
            }
        });
    ui.add_space(4.0);
}

fn attachments(ui: &mut Ui, body: &MessageBody, action: &mut Option<Action>) {
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(format!("{} ", glyphs::SAVE)).weak());
        for (index, attachment) in body.attachments.iter().enumerate() {
            let label = format!(
                "{}  {}",
                attachment.filename,
                format_size(attachment.data.len())
            );
            if ui
                .add(Button::new(label).size(ButtonSize::Small).outline())
                .on_hover_text(&attachment.mime)
                .clicked()
            {
                *action = Some(Action::SaveAttachment(index));
            }
        }
    });
    ui.add_space(4.0);
}

/// Shows the headers and the sanitized markup, for when a message renders
/// oddly and the user wants to know why.
fn source_view(
    ui: &mut Ui,
    prepared: Option<&Prepared>,
    body: Option<&MessageBody>,
    base_size: f32,
) {
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if let Some(body) = body {
            ui.horizontal(|ui| {
                ui.add(Badge::new("headers", BadgeTone::Neutral));
                ui.label(RichText::new(format_size(body.raw_size)).weak().small());
            });
            ui.add_space(4.0);
            let headers: String = body
                .headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}\n"))
                .collect();
            ui.add(
                egui::Label::new(RichText::new(headers).monospace().size(base_size * 0.85))
                    .selectable(true),
            );
            ui.add_space(12.0);
        }

        if let Some(prepared) = prepared {
            ui.add(Badge::new("sanitized html", BadgeTone::Neutral));
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(
                    RichText::new(&prepared.html).monospace().size(base_size * 0.85),
                )
                .selectable(true),
            );
        } else if let Some(body) = body {
            if let Some(text) = &body.text {
                ui.add(Badge::new("text/plain", BadgeTone::Neutral));
                ui.add_space(4.0);
                ui.add(
                    egui::Label::new(
                        RichText::new(text).monospace().size(base_size * 0.85),
                    )
                    .selectable(true),
                );
            }
        }
        ui.add_space(24.0);
    });
}

fn join_addrs(addrs: &[crate::mail::Addr]) -> String {
    const MAX: usize = 6;
    let shown: Vec<String> = addrs.iter().take(MAX).map(|a| a.short().to_string()).collect();
    if addrs.len() > MAX {
        format!("{}, +{} more", shown.join(", "), addrs.len() - MAX)
    } else {
        shown.join(", ")
    }
}
