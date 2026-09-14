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
    /// Font this pane draws in.
    pub font: egui::FontId,
    pub show_source: &'a mut bool,
    /// The body has been requested but has not arrived.
    pub loading: bool,
    pub theme: &'a Theme,
}

pub fn show(ui: &mut Ui, input: ReaderInput<'_>) -> Option<Action> {
    let Some(envelope) = input.envelope else {
        return empty_state(ui, input.theme);
    };

    let mut action = None;

    header(ui, envelope, input.theme, &mut action, input.show_source);
    ui.separator();

    if let Some(prepared) = input.prepared
        && prepared.blocked_remote > 0
        && !input.allow_remote
    {
        let sender = envelope.from.first().map(|a| a.short());
        privacy_notice(ui, prepared.blocked_remote, sender, &mut action);
    }

    if let Some(body) = input.body
        && !body.attachments.is_empty()
    {
        attachments(ui, body, &mut action);
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
        source_view(ui, input.prepared, input.body, input.font.size);
        return action;
    }

    let Some(prepared) = input.prepared else { return action };
    let Some(body) = input.body else { return action };

    egui::ScrollArea::vertical().id_salt(("body", envelope.uid)).auto_shrink([false, false]).show(
        ui,
        |ui| {
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
            let font = input.font.clone();
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
        },
    );

    action
}

fn empty_state(ui: &mut Ui, theme: &Theme) -> Option<Action> {
    ui.add_space(ui.available_height() * 0.35);
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(glyphs::FOLDER_OPEN.to_string())
                .size(36.0)
                .color(ui.visuals().weak_text_color()),
        );
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

    let subject =
        if envelope.subject.trim().is_empty() { "(no subject)" } else { &envelope.subject };
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
                        egui::Label::new(theme.muted_text(format!("<{}>", from.email))).truncate(),
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
                    .size(ButtonSize::Medium)
                    .accent(Accent::Blue),
            )
            .clicked()
        {
            *action = Some(Action::Reply { all: false });
        }
        if ui.add(Button::new("Reply all").size(ButtonSize::Medium).outline()).clicked() {
            *action = Some(Action::Reply { all: true });
        }
        if ui
            .add(
                Button::new(format!("{} Forward", glyphs::ARROW_RIGHT))
                    .size(ButtonSize::Medium)
                    .outline(),
            )
            .clicked()
        {
            *action = Some(Action::Forward);
        }
        if ui
            .add(
                Button::new(format!("{} Archive", glyphs::FOLDER))
                    .size(ButtonSize::Medium)
                    .outline(),
            )
            .clicked()
        {
            *action = Some(Action::Archive);
        }
        if ui
            .add(
                // No accent: `outline` has no fill to colour, and asking
                // for red here only looked like it said something. Deleting
                // is undone by the trash folder, not by shouting about it,
                // and a toolbar whose loudest button is Delete is worse than
                // one where it reads as what it is — another thing to do
                // with the message.
                Button::new(format!("{} Delete", glyphs::TRASH)).size(ButtonSize::Medium).outline(),
            )
            .clicked()
        {
            *action = Some(Action::Delete);
        }

        // What to do with the message, then what to do with the view of it,
        // divided by a rule rather than by pushing the second group to the
        // right. A right-to-left child does not wrap: handed less width than
        // its buttons need — which a narrow reading pane does — it draws
        // leftwards from its own right edge, straight over the buttons
        // already there.
        ui.separator();

        let label = if *show_source { "Rendered" } else { "Source" };
        if ui.add(Button::new(label).size(ButtonSize::Medium).outline()).clicked() {
            *show_source = !*show_source;
        }
        // The reader draws a sanitized block model, which is the right
        // trade for most mail and the wrong one for a message built as a
        // page. The browser has the whole of CSS; this hands it the same
        // sanitized document, with the `cid:` parts inlined so it stands on
        // its own — and with the remote content this message is still
        // withholding still withheld.
        if ui
            .add(
                Button::new(format!("{} Browser", super::icons::EXTERNAL))
                    .size(ButtonSize::Medium)
                    .outline(),
            )
            .on_hover_text(
                "Open in your browser, where it renders as sent \u{2014} and where printing lives",
            )
            .clicked()
        {
            *action = Some(Action::OpenInBrowser);
        }
    });
    ui.add_space(6.0);
}

/// Explains what was withheld and offers to load it. Worth being explicit
/// about: loading these tells the sender the message was opened.
///
/// The two choices differ in scope, so they are separate buttons. Loading
/// this message reveals only what opening it already revealed. Trusting the
/// sender also reveals future opens, before you have decided on them.
fn privacy_notice(ui: &mut Ui, blocked: usize, sender: Option<&str>, action: &mut Option<Action>) {
    ui.add_space(6.0);
    // `multiline`, because the body is a sentence rather than a few words:
    // the default layout puts it on the title's row and expects to truncate
    // it, and what it did instead was draw it over the top.
    Callout::new(CalloutTone::Info)
        .icon(glyphs::EYE_OFF.to_string())
        .title(format!(
            "{} remote {} blocked",
            blocked,
            if blocked == 1 { "image" } else { "images" }
        ))
        .body("Loading them tells the sender you opened this message.")
        .multiline()
        .tinted()
        // The buttons go under the notice rather than in its action area,
        // which is right-aligned on the title's row and does not wrap: on a
        // reading pane narrower than about five hundred points there is not
        // room for both, and what does not fit is drawn over the title.
        .show(ui, |_| {});

    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        if ui
            .add(Button::new("Load images").size(ButtonSize::Small))
            .on_hover_text("Remembered for this message")
            .clicked()
        {
            *action = Some(Action::LoadRemoteImages);
        }
        if let Some(sender) = sender
            && ui
                .add(Button::new("Always from sender").size(ButtonSize::Small).outline())
                .on_hover_text(format!(
                    "Load remote content from {sender} without asking, including \
                     in messages you have not opened yet"
                ))
                .clicked()
        {
            *action = Some(Action::AllowRemoteSender);
        }
    });
    ui.add_space(4.0);
}

fn attachments(ui: &mut Ui, body: &MessageBody, action: &mut Option<Action>) {
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(format!("{} ", glyphs::SAVE)).weak());
        for (index, attachment) in body.attachments.iter().enumerate() {
            let label = format!("{}  {}", attachment.filename, format_size(attachment.data.len()));
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
            let headers: String =
                body.headers.iter().map(|(name, value)| format!("{name}: {value}\n")).collect();
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
                egui::Label::new(RichText::new(&prepared.html).monospace().size(base_size * 0.85))
                    .selectable(true),
            );
        } else if let Some(body) = body
            && let Some(text) = &body.text
        {
            ui.add(Badge::new("text/plain", BadgeTone::Neutral));
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(RichText::new(text).monospace().size(base_size * 0.85))
                    .selectable(true),
            );
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

#[cfg(test)]
mod tests {
    /// The notice explaining blocked images once drew its body, and both its
    /// buttons, on top of its own title. Two mistakes at once: a body that is
    /// a sentence needs `multiline`, and the action area is right-aligned on
    /// the title's row and does not wrap.
    ///
    /// Measured on the callout rather than on the pane, for the reason the
    /// test below gives: egui keeps laid-out widget rects to itself.
    #[test]
    fn a_notice_with_a_sentence_and_buttons_needs_both_of_them() {
        let theme = crate::config::ThemeChoice::Outlook.theme();
        let title = "11 remote images blocked";
        let body = "Loading them tells the sender you opened this message.";

        // How tall the notice comes out. Overlapping text is text that was
        // not given a line of its own, so the broken arrangements are the
        // short ones.
        let height = |multiline: bool, buttons_inside: bool| {
            crate::ui::raster::measure(&theme, egui::vec2(420.0, 300.0), |ui| {
                let add = |ui: &mut egui::Ui| {
                    ui.add(elegance::Button::new("Load images").size(elegance::ButtonSize::Small));
                    ui.add(
                        elegance::Button::new("Always from sender")
                            .size(elegance::ButtonSize::Small)
                            .outline(),
                    );
                };
                let mut callout = elegance::Callout::new(elegance::CalloutTone::Info)
                    .icon(elegance::glyphs::EYE_OFF.to_string())
                    .title(title)
                    .body(body)
                    .tinted();
                if multiline {
                    callout = callout.multiline();
                }
                if buttons_inside {
                    callout.show(ui, add);
                } else {
                    callout.show(ui, |_| {});
                    ui.horizontal_wrapped(add);
                }
                ui.min_rect().height()
            })
        };

        let everything_on_one_row = height(false, true);
        let body_given_its_own_line = height(true, true);
        let and_the_buttons_too = height(false, false);
        let both = height(true, false);

        assert!(
            body_given_its_own_line > everything_on_one_row,
            "`multiline` did not give the body a line of its own"
        );
        assert!(
            and_the_buttons_too > everything_on_one_row,
            "moving the buttons out did not give them a row of their own"
        );
        assert!(
            both > body_given_its_own_line && both > and_the_buttons_too,
            "the arrangement the reader uses is no taller than the ones missing half of it: \
             {both} against {body_given_its_own_line} and {and_the_buttons_too}"
        );
    }

    /// Why the reader's toolbar does not push its last two buttons to the
    /// right, which is the obvious way to write it and what it used to do.
    ///
    /// A right-to-left child draws from its own right edge leftwards and does
    /// not wrap. Inside a wrapped row it is handed whatever width is left,
    /// and when that is less than its buttons need it draws over the ones
    /// already on the row. A narrow reading pane is exactly that case.
    ///
    /// This measures the two arrangements rather than the pane: egui keeps
    /// laid-out widget rects to itself, so a test cannot ask the real header
    /// where its buttons went. What it guards is the reasoning — the trap is
    /// easy to walk back into, and it looks correct at any width where the
    /// row happens not to be full.
    #[test]
    fn a_right_to_left_group_cannot_share_a_wrapped_row() {
        let theme = crate::config::ThemeChoice::Outlook.theme();
        let labels = ["Reply", "Reply all", "Forward", "Archive", "Delete"];
        let trailing = ["Source", "Print"];

        let overlaps = |right_to_left: bool| {
            let mut widths = Vec::new();
            for width in (200..=760).step_by(20).map(|w| w as f32) {
                let rects = crate::ui::raster::measure(&theme, egui::vec2(width, 400.0), |ui| {
                    let mut rects: Vec<egui::Rect> = Vec::new();
                    ui.horizontal_wrapped(|ui| {
                        let button = |ui: &mut egui::Ui, label: &str| {
                            ui.add(
                                elegance::Button::new(label)
                                    .size(elegance::ButtonSize::Small)
                                    .outline(),
                            )
                            .rect
                        };
                        for label in labels {
                            rects.push(button(ui, label));
                        }
                        if right_to_left {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    for label in trailing {
                                        rects.push(button(ui, label));
                                    }
                                },
                            );
                        } else {
                            ui.separator();
                            for label in trailing {
                                rects.push(button(ui, label));
                            }
                        }
                    });
                    rects
                });

                let clash = rects
                    .iter()
                    .enumerate()
                    .any(|(i, a)| rects.iter().skip(i + 1).any(|b| a.intersects(*b)));
                if clash {
                    widths.push(width);
                }
            }
            widths
        };

        let pushed_right = overlaps(true);
        assert!(
            !pushed_right.is_empty(),
            "the arrangement this is a warning about did not misbehave, so it is no longer \
             a warning about anything"
        );

        let in_the_row = overlaps(false);
        assert!(
            in_the_row.is_empty(),
            "the arrangement the reader uses overlapped at {in_the_row:?}"
        );
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;

    /// Writes a picture of the reader's header, for looking at the button row
    /// without launching the application. Two widths, because how it wraps is
    /// most of what there is to look at.
    ///
    ///     REMAIL_RENDER=/tmp cargo test render_header -- --ignored
    #[test]
    #[ignore = "writes a file; run it when you want to look at something"]
    fn render_header() {
        let dir = std::env::var("REMAIL_RENDER").unwrap_or_else(|_| "/tmp".into());
        let theme = crate::config::ThemeChoice::Outlook.theme();
        let envelope = Envelope {
            subject: "2026 ALTA Fall Pickleball League".into(),
            from: vec![crate::mail::Addr {
                name: "Bonny Robichaud".into(),
                email: "bonny@example.com".into(),
            }],
            date: 1789238659,
            ..Default::default()
        };
        for width in [620.0_f32, 380.0] {
            crate::ui::raster::render(
                &format!("{dir}/header_{}.png", width as u32),
                &theme,
                width,
                260.0,
                2.0,
                |ui| {
                    let mut action = None;
                    let mut show_source = false;
                    header(ui, &envelope, &theme, &mut action, &mut show_source);
                },
            );
        }
    }
}
