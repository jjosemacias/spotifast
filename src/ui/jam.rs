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
    if matches!(intent, Intent::AskJoin) {
        ui.ctx()
            .data_mut(|data| data.insert_temp(egui::Id::new("jam-join-trigger"), response.id));
    }
    for (matches, key) in [
        (
            matches!(intent, Intent::OpenInvitation),
            "jam-invitation-trigger",
        ),
        (matches!(intent, Intent::ExpandQr), "jam-qr-trigger"),
    ] {
        if matches {
            ui.ctx()
                .data_mut(|data| data.insert_temp(egui::Id::new(key), response.id));
        }
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

fn heading(app: &App) -> String {
    if let Some(name) = app.jam.host_name() {
        gettext(app.locale, "Jam hosted by {name}").replace("{name}", name)
    } else {
        gettext(app.locale, title(app)).to_string()
    }
}

fn participants(app: &mut App, ui: &mut egui::Ui) {
    let Some(members) = app.jam.metadata.members.clone() else {
        text(app, ui, "Participants unavailable");
        return;
    };
    text(app, ui, "Participants · Partial list");
    if members.is_empty() {
        text(
            app,
            ui,
            "No participants listed in this response. The full list is unconfirmed.",
        );
    }
    let owner = app.jam.metadata.owner.clone();
    for (index, member) in members.iter().enumerate() {
        // Identity is established by the parser, never by name or avatar. Stable
        // UI order is the supplied partial snapshot, with no fabricated total.
        ui.push_id(
            (
                "jam-member",
                member.id.as_ref().or(member.username.as_ref()),
                index,
            ),
            |ui| {
                ui.horizontal_top(|ui| {
                    super::widgets::cover(
                        ui,
                        &app.palette,
                        member.image.as_deref(),
                        28.0,
                        14.0,
                        theme::Icon::User,
                    );
                    ui.vertical(|ui| {
                        let name = member
                            .name
                            .clone()
                            .unwrap_or_else(|| gettext(app.locale, "Participant").to_string());
                        let galley = crate::bidi::layout(
                            ui.painter(),
                            &name,
                            theme::regular(14.0),
                            app.palette.text,
                            ui.available_width(),
                            2,
                            Some(crate::bidi::ELLIPSIS),
                        );
                        ui.add(egui::Label::new(galley)).on_hover_text(name);
                        if owner.as_deref().is_some_and(|id| member.named_by(id)) {
                            text(app, ui, "Host (declared by Spotify)");
                        } else if owner.is_some() {
                            text(app, ui, "Participant · Permissions unconfirmed");
                        } else {
                            text(app, ui, "Role unconfirmed");
                        }
                    });
                });
            },
        );
    }
}

/// Paint modules directly, with a four-module quiet zone. No texture, URI cache,
/// disk file, network service or diagnostic representation holds the QR.
fn qr(ui: &mut egui::Ui, code: &qrcodegen::QrCode, desired: f32) {
    let modules = code.size() + 8;
    let pixels = (desired.min(ui.available_width()) / modules as f32)
        .floor()
        .max(1.0);
    let side = pixels * modules as f32;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Image, true, "Jam invitation QR")
    });
    ui.painter().rect_filled(rect, 0.0, egui::Color32::WHITE);
    for y in 0..code.size() {
        for x in 0..code.size() {
            if code.get_module(x, y) {
                let min = rect.min + egui::vec2((x + 4) as f32 * pixels, (y + 4) as f32 * pixels);
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(min, egui::Vec2::splat(pixels)),
                    0.0,
                    egui::Color32::BLACK,
                );
            }
        }
    }
}

fn return_focus(ctx: &egui::Context, key: &'static str, trigger: &'static str) {
    let focus = ctx
        .data_mut(|data| data.remove_temp::<bool>(egui::Id::new(key)))
        .unwrap_or(false);
    if focus && let Some(id) = ctx.data(|data| data.get_temp::<egui::Id>(egui::Id::new(trigger))) {
        ctx.memory_mut(|memory| memory.request_focus(id));
    }
}

fn invitation(app: &mut App, ui: &mut egui::Ui) {
    if app.jam.invitation().is_none() {
        text(app, ui, "Invitation unavailable");
        return;
    }
    button(app, ui, "Invite", true, Intent::OpenInvitation);
    return_focus(ui.ctx(), "jam-invitation-return", "jam-invitation-trigger");
    return_focus(ui.ctx(), "jam-qr-return", "jam-qr-trigger");
    if !app.jam.invitation_open {
        return;
    }
    egui::Frame::new()
        .fill(app.palette.surface)
        .inner_margin(12.0)
        .corner_radius(8.0)
        .show(ui, |ui| {
            let title = ui.add(
                egui::Label::new(gettext(app.locale, "Invite to this Jam"))
                    .sense(egui::Sense::focusable_noninteractive()),
            );
            if ui
                .ctx()
                .data_mut(|data| data.remove_temp::<bool>(egui::Id::new("jam-invitation-focus")))
                .unwrap_or(false)
            {
                title.request_focus();
                title.scroll_to_me(Some(egui::Align::Min));
            }
            text(app, ui, "Share only with people you want to invite.");
            if let Some(code) = &app.jam.qr {
                qr(ui, code, 180.0);
            } else {
                text(
                    app,
                    ui,
                    "QR unavailable. You can request a copy of the link.",
                );
            }
            ui.horizontal_wrapped(|ui| {
                button(app, ui, "Copy link", true, Intent::CopyInvitation);
                button(
                    app,
                    ui,
                    "Enlarge QR",
                    app.jam.qr.is_some(),
                    Intent::ExpandQr,
                );
                button(app, ui, "Close invitation", true, Intent::CloseInvitation);
            });
            if !app.jam.qr_expanded
                && ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
            {
                enqueue(app, Intent::CloseInvitation);
            }
        });
}

fn qr_modal(app: &mut App, ctx: &egui::Context) {
    if !app.jam.qr_expanded || app.jam.invitation().is_none() {
        return;
    }
    let response = egui::Modal::new(egui::Id::new("jam-qr-modal")).show(ctx, |ui| {
        ui.set_width(340.0_f32.min(ctx.content_rect().width() - 48.0));
        egui::ScrollArea::vertical()
            .max_height((ctx.content_rect().height() - 80.0).max(80.0))
            .show(ui, |ui| {
                text(app, ui, "Invite to this Jam");
                if let Some(code) = &app.jam.qr {
                    qr(ui, code, 300.0);
                }
                let close = ui.add(
                    egui::Button::new(gettext(app.locale, "Close QR"))
                        .min_size(egui::vec2(36.0, 36.0)),
                );
                if !ctx.data(|data| {
                    data.get_temp::<bool>(egui::Id::new("jam-qr-focused"))
                        .unwrap_or(false)
                }) {
                    close.request_focus();
                    ctx.data_mut(|data| data.insert_temp(egui::Id::new("jam-qr-focused"), true));
                }
                if close.clicked() {
                    enqueue(app, Intent::CloseQr);
                }
                button(app, ui, "Copy link", true, Intent::CopyInvitation);
            });
    });
    if response.should_close() {
        enqueue(app, Intent::CloseQr);
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
                Kind::Join => {
                    "Joining Jam… Requested: listen on this computer. Audio and controls are unconfirmed."
                }
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
                crate::backend::jam::Error::WriteUncertain => {
                    "A previous Jam request may still complete. Checking state cannot safely enable another change. Jam changes remain blocked here."
                }
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
    let heading = if fresh {
        heading(app)
    } else {
        gettext(app.locale, "Jam").to_string()
    };
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
    if absent {
        button(
            app,
            ui,
            "Join with an invitation…",
            app.local.connected,
            Intent::AskJoin,
        );
    }
    progress(app, ui);
    ui.add_space(8.0);
}

pub(super) fn section(app: &mut App, ui: &mut egui::Ui) {
    deadline(app, ui.ctx());
    let name = heading(app);
    let galley = crate::bidi::layout(
        ui.painter(),
        &name,
        theme::bold(20.0),
        app.palette.text,
        ui.available_width(),
        2,
        Some(crate::bidi::ELLIPSIS),
    );
    let heading = ui
        .add(egui::Label::new(galley).sense(egui::Sense::focusable_noninteractive()))
        .on_hover_text(name);
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
                button(
                    app,
                    ui,
                    "Join with an invitation…",
                    app.local.connected,
                    Intent::AskJoin,
                );
                if !app.local.connected {
                    text(app, ui, "Set up playback on this computer before joining.");
                }
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
                participants(app, ui);
                invitation(app, ui);
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
                text(app, ui, "Permissions unconfirmed.");
                text(app, ui, "Playback queue; shared queue unconfirmed.");
                text(app, ui, "Audio state is unconfirmed");
                text(
                    app,
                    ui,
                    if p.is_listening {
                        "Spotify declares listening: yes"
                    } else {
                        "Spotify declares listening: no"
                    },
                );
                text(
                    app,
                    ui,
                    if p.is_controlling {
                        "Spotify declares control: yes"
                    } else {
                        "Spotify declares control: no"
                    },
                );
                text(app, ui, "Effective mode and host controls are unconfirmed.");
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
            let label = if confirmed {
                if app.jam.host_name().is_some() {
                    let role = gettext(
                        app.locale,
                        if app.jam.owner_id().is_some() {
                            "Host"
                        } else {
                            "Participant"
                        },
                    );
                    format!("{} · {role}", heading(app))
                } else {
                    gettext(
                        app.locale,
                        if app.jam.owner_id().is_some() {
                            "Your Jam · Host"
                        } else {
                            "Jam · Participant"
                        },
                    )
                    .to_string()
                }
            } else {
                gettext(app.locale, "Jam · State unconfirmed").to_string()
            };
            let mut manage = false;
            egui::Sides::new().shrink_left().show(
                ui,
                |ui| {
                    theme::text(ui, label.clone(), theme::bold(13.0), foreground)
                        .on_hover_text(label);
                },
                |ui| {
                    let label = if confirmed {
                        "Manage Jam"
                    } else {
                        "Check Jam state"
                    };
                    manage = ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(gettext(app.locale, label)).color(foreground),
                            )
                            .frame(false)
                            .min_size(egui::vec2(36.0, 36.0)),
                        )
                        .clicked();
                },
            );
            if manage {
                enqueue(app, Intent::Manage);
            }
        });
}

pub(super) fn confirmation(app: &mut App, ctx: &egui::Context) {
    qr_modal(app, ctx);
    if !app.jam.qr_expanded {
        ctx.data_mut(|data| data.remove::<bool>(egui::Id::new("jam-qr-focused")));
    }
    join_form(app, ctx);
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

fn join_form(app: &mut App, ctx: &egui::Context) {
    let Some(draft) = &app.jam.join else {
        let restore = ctx.data_mut(|data| {
            let stamp = data.get_temp::<crate::backend::jam::lifecycle::Stamp>(egui::Id::new(
                "jam-join-return",
            ));
            data.remove::<crate::backend::jam::lifecycle::Stamp>(egui::Id::new("jam-join-return"));
            stamp
        });
        if restore == Some(app.jam.stamp)
            && app.jam.can_create()
            && let Some(trigger) =
                ctx.data(|data| data.get_temp::<egui::Id>(egui::Id::new("jam-join-trigger")))
        {
            ctx.memory_mut(|memory| memory.request_focus(trigger));
        }
        return;
    };
    let (mut value, listen_here, invalid) = (draft.link.clone(), draft.listen_here, draft.invalid);
    let first = ctx.data_mut(|data| {
        let first = data
            .get_temp::<bool>(egui::Id::new("jam-join-focus"))
            .unwrap_or(false);
        data.remove::<bool>(egui::Id::new("jam-join-focus"));
        first
    });
    let modal = egui::Modal::new(egui::Id::new("jam-join-form")).show(ctx, |ui| {
        ui.set_width(360.0_f32.min(ctx.content_rect().width() - 48.0));
        egui::ScrollArea::vertical().max_height((ctx.content_rect().height() - 120.0).max(120.0)).show(ui, |ui| {
            ui.heading(gettext(app.locale, "Join with an invitation"));
            text(app, ui, "Invitation link");
            let field = ui.add(egui::TextEdit::singleline(&mut value)
                .id(egui::Id::new("jam-join-link"))
                .min_size(egui::vec2(36.0, 36.0))
                .margin(egui::Margin::symmetric(4, 10))
                .char_limit(2048).desired_width(f32::INFINITY)
                .hint_text(gettext(app.locale, "Paste a Spotify Jam long link")));
            field.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, true, gettext(app.locale, "Invitation link")));
            if first { field.request_focus(); }
            if field.changed() {
                enqueue(app, Intent::EditJoin(crate::model::jam::DraftText(value.clone())));
            }
            if invalid { text(app, ui, "Check the invitation link. Only Spotify HTTPS long links are supported here."); }
            text(app, ui, "Paste only when you choose. Short links and other formats are pending support.");
            if ui.add_sized(egui::vec2(ui.available_width(), 36.0), egui::RadioButton::new(listen_here, gettext(app.locale, "Request listening on this computer"))).clicked() {
                enqueue(app, Intent::RequestListenHere);
            }
            text(app, ui, "This experimental request asks to listen and control. Spotify may change membership or playback. Effective audio, synchronization and host controls cannot be confirmed here.");
            ui.add_enabled(false, egui::RadioButton::new(false, gettext(app.locale, "Control the host without local audio")));
            text(app, ui, "This mode is not available until its contract is verified.");
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                button(app, ui, "Cancel", true, Intent::CancelJoin);
                button(app, ui, "Join", listen_here && app.jam.can_create() && app.local.connected, Intent::ConfirmJoin);
            });
        });
    });
    if modal.should_close() {
        enqueue(app, Intent::CancelJoin);
    }
}
