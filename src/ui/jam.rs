//! Jam management shares the existing queue; views only emit application actions.
use crate::{
    app::App,
    backend::jam::{Membership, lifecycle::Kind},
    i18n::gettext,
    model::{Action, jam::Intent},
    theme,
};

pub(super) fn enqueue(app: &mut App, intent: Intent) {
    app.actions.push(Action::Jam(crate::model::jam::UiAction {
        intent,
        expected: app.jam.stamp,
    }));
}

fn text(app: &App, ui: &mut egui::Ui, value: &'static str) {
    let value = gettext(app.locale, value);
    let galley = crate::bidi::layout(
        ui.painter(),
        &value,
        theme::regular(13.0),
        app.palette.secondary,
        ui.available_width(),
        usize::MAX,
        None,
    );
    ui.add(egui::Label::new(galley));
}
fn button(app: &mut App, ui: &mut egui::Ui, label: &'static str, enabled: bool, intent: Intent) {
    let label = egui::RichText::new(gettext(app.locale, label));
    let label = if matches!(intent, Intent::Create) {
        label.color(accent_ink(&app.palette))
    } else {
        label
    };
    let mut control = egui::Button::new(label)
        .wrap()
        .min_size(egui::vec2(36.0, 36.0));
    if matches!(intent, Intent::Create) {
        control = control.fill(app.palette.accent);
    }
    let response = ui.add_enabled(enabled, control);
    if matches!(intent, Intent::AskEnd) {
        ui.ctx()
            .data_mut(|data| data.insert_temp(egui::Id::new("jam-end-trigger"), response.id));
    }
    if response.clicked() {
        enqueue(app, intent);
    }
}

fn deadline(app: &App, ctx: &egui::Context) {
    if let Some(until) = app.jam.fresh_until {
        let now = tokio::time::Instant::now();
        if until > now {
            ctx.request_repaint_after(until.saturating_duration_since(now));
        }
    }
}

fn title(app: &App) -> &'static str {
    match &app.jam.membership {
        Membership::Present(p) if app.jam.fresh() && p.is_session_owner => "Your Jam",
        Membership::Present(_) if app.jam.fresh() => "Jam",
        Membership::Absent if app.jam.fresh() => "Listen and choose together",
        _ => "Jam state is unconfirmed",
    }
}

fn progress(app: &App, ui: &mut egui::Ui) {
    if let Some(kind) = app.jam.pending {
        text(
            app,
            ui,
            match kind {
                Kind::Current => "Checking Jam…",
                Kind::Create => "Creating Jam…",
                Kind::End => "Ending Jam…",
            },
        );
    }
    if let Some(notice) = app.jam.notice {
        text(app, ui, notice);
    }
    if let Some(error) = app.jam.error {
        text(
            app,
            ui,
            match error {
                crate::backend::jam::Error::Http(_) => {
                    "Spotify rejected the Jam request. Check its state before trying again."
                }
                _ => "We could not confirm the Jam state. Check it before trying again.",
            },
        );
    }
}

fn create(app: &mut App, ui: &mut egui::Ui) {
    button(
        app,
        ui,
        "Start a Jam",
        app.jam.can_create() && app.local.connected && app.local.is_active(),
        Intent::Create,
    );
    if !app.local.connected || !app.local.is_active() {
        text(app, ui, "Play on this computer before creating a Jam.");
    }
}

pub(super) fn summary(app: &mut App, ui: &mut egui::Ui) {
    deadline(app, ui.ctx());
    let palette = app.palette;
    let fresh = app.jam.fresh();
    let absent = app.jam.can_create();
    let present = fresh && matches!(app.jam.membership, Membership::Present(_));
    let heading = gettext(app.locale, if fresh { title(app) } else { "Jam" });
    let status = gettext(app.locale, "State unconfirmed");
    let local = app.local.connected && app.local.is_active();
    egui::Sides::new().shrink_left().show(
        ui,
        |ui| {
            theme::text(ui, heading, theme::bold(14.0), palette.text);
            if !fresh {
                theme::text(ui, status, theme::regular(13.0), palette.secondary);
            }
        },
        |ui| {
            if absent {
                button(app, ui, "Start a Jam", local, Intent::Create);
            } else if present {
                button(app, ui, "Manage Jam", true, Intent::Manage);
            } else {
                button(
                    app,
                    ui,
                    "Check Jam state",
                    app.jam.pending.is_none(),
                    Intent::Current,
                );
            }
        },
    );
    if absent && !local {
        text(app, ui, "Play on this computer before creating a Jam.");
    }
    progress(app, ui);
    ui.add_space(8.0);
}

pub(super) fn section(app: &mut App, ui: &mut egui::Ui) {
    deadline(app, ui.ctx());
    let heading = ui.add(
        egui::Label::new(
            egui::RichText::new(gettext(app.locale, title(app)))
                .font(theme::bold(20.0))
                .color(app.palette.text),
        )
        .sense(egui::Sense::focusable_noninteractive()),
    );
    let focus = ui.ctx().data_mut(|data| {
        let focus = data
            .get_temp::<bool>(egui::Id::new("jam-management-focus"))
            .unwrap_or(false);
        data.remove::<bool>(egui::Id::new("jam-management-focus"));
        focus
    });
    if focus {
        heading.request_focus();
        heading.scroll_to_me(Some(egui::Align::Min));
    }
    ui.add_space(8.0);
    progress(app, ui);
    if app.jam.fresh() {
        match app.jam.membership.clone() {
            Membership::Absent => {
                create(app, ui);
                text(
                    app,
                    ui,
                    "Joining with an invitation is not available yet. Use Spotify to join.",
                );
            }
            Membership::Present(p) => {
                text(
                    app,
                    ui,
                    if p.is_session_owner {
                        "Host"
                    } else {
                        "Participant"
                    },
                );
                text(app, ui, "Participants unavailable");
                if p.is_session_owner {
                    button(
                        app,
                        ui,
                        "End Jam…",
                        app.jam.owner_id().is_some(),
                        Intent::AskEnd,
                    );
                } else {
                    text(
                        app,
                        ui,
                        "Leaving this Jam is not available here yet. Use Spotify to leave; the Jam can continue for others.",
                    );
                }
                text(app, ui, "Invitation unavailable. Permissions unconfirmed.");
                text(app, ui, "Local queue; shared queue unconfirmed.");
                text(app, ui, "Audio state is unconfirmed");
            }
            Membership::Unknown => (),
        }
    } else if app.jam.pending.is_none() {
        text(
            app,
            ui,
            "Check the Jam state before continuing. Audio state is unconfirmed.",
        );
    }
    if !app.jam.fresh() || app.jam.error.is_some() {
        button(
            app,
            ui,
            "Check Jam state",
            app.jam.pending.is_none(),
            Intent::Current,
        );
    }
    ui.add_space(12.0);
    ui.separator();
    ui.add_space(8.0);
}

pub(crate) fn bar_visible(app: &App) -> bool {
    !(app.jam.fresh() && matches!(app.jam.membership, Membership::Absent))
        && (app.jam.engaged || matches!(app.jam.membership, Membership::Present(_)))
}

fn accent_ink(palette: &theme::Palette) -> egui::Color32 {
    let luminance = |color: egui::Color32| {
        let linear = |v: u8| {
            let v = f32::from(v) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
    };
    let background = luminance(palette.accent);
    // Preserve the theme's semantic pair when readable, including custom themes.
    for ink in [
        palette.on_accent,
        theme::Palette::dark().on_accent,
        egui::Color32::BLACK,
        egui::Color32::WHITE,
    ] {
        let foreground = luminance(ink);
        if (foreground.max(background) + 0.05) / (foreground.min(background) + 0.05) >= 4.5 {
            return ink;
        }
    }
    palette.on_accent
}

pub(super) fn bar(app: &mut App, ui: &mut egui::Ui) {
    deadline(app, ui.ctx());
    if !bar_visible(app) {
        return;
    }
    let confirmed = app.jam.fresh() && matches!(app.jam.membership, Membership::Present(_));
    let palette = app.palette;
    let ink = accent_ink(&palette);
    let foreground = if confirmed { ink } else { palette.text };
    egui::Panel::bottom("jam-bar")
        .exact_size(36.0)
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::new()
                .fill(if confirmed {
                    palette.accent
                } else {
                    palette.surface
                })
                .inner_margin(egui::Margin::symmetric(12, 0)),
        )
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                let label = if confirmed {
                    if app.jam.owner_id().is_some() {
                        "Your Jam · Host"
                    } else {
                        "Jam · Participant"
                    }
                } else {
                    "Jam · State unconfirmed"
                };
                theme::text(
                    ui,
                    gettext(app.locale, label),
                    theme::bold(13.0),
                    foreground,
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if confirmed {
                        "Manage Jam"
                    } else {
                        "Check Jam state"
                    };
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(gettext(app.locale, label)).color(foreground),
                            )
                            .frame(false)
                            .min_size(egui::vec2(36.0, 36.0)),
                        )
                        .clicked()
                    {
                        // Open recovery, never implicitly read Current.
                        enqueue(app, Intent::Manage);
                    }
                });
            });
        });
}

pub(super) fn confirmation(app: &mut App, ctx: &egui::Context) {
    let focus_id = egui::Id::new("jam-end-focus");
    let Some(confirmation) = &app.jam.confirmation else {
        ctx.data_mut(|data| data.remove::<bool>(focus_id));
        let return_stamp = ctx.data_mut(|data| {
            let stamp = data.get_temp::<crate::backend::jam::lifecycle::Stamp>(egui::Id::new(
                "jam-return-focus",
            ));
            data.remove::<crate::backend::jam::lifecycle::Stamp>(egui::Id::new("jam-return-focus"));
            stamp
        });
        if return_stamp == Some(app.jam.stamp)
            && app.jam.owner_id().is_some()
            && let Some(trigger) =
                ctx.data(|data| data.get_temp::<egui::Id>(egui::Id::new("jam-end-trigger")))
        {
            ctx.memory_mut(|memory| memory.request_focus(trigger));
        }
        return;
    };
    if confirmation.stamp != app.backend.jam_stamp()
        || app.jam.owner_id() != Some(confirmation.session_id.as_str())
    {
        enqueue(app, Intent::CancelEnd);
        return;
    }
    let first = !ctx
        .data(|data| data.get_temp::<bool>(focus_id))
        .unwrap_or(false);
    let modal = egui::Modal::new(egui::Id::new("jam-end-confirmation")).show(ctx, |ui| {
        ui.set_width(320.0_f32.min(ctx.content_rect().width() - 48.0));
        ui.heading(gettext(app.locale, "End this Jam for everyone?"));
        text(
            app,
            ui,
            "This requests an end for everyone. We can only verify your own membership; your absence does not confirm that the Jam ended for others. Audio and invitation status are unconfirmed.",
        );
        ui.add_space(12.0);
        ui.horizontal_wrapped(|ui| {
            let cancel = ui.add(
                egui::Button::new(gettext(app.locale, "Cancel")).min_size(egui::vec2(36.0, 36.0)),
            );
            if first {
                cancel.request_focus();
            }
            if cancel.clicked() {
                enqueue(app, Intent::CancelEnd);
            }
            button(
                app,
                ui,
                "End for everyone",
                app.jam.owner_id().is_some(),
                Intent::ConfirmEnd,
            );
        });
    });
    ctx.data_mut(|data| data.insert_temp(focus_id, true));
    if modal.should_close() {
        enqueue(app, Intent::CancelEnd);
    }
}
