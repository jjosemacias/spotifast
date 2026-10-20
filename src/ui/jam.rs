//! Jam lives inside Connect; this view only emits application actions.
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
    let response = ui.add_enabled(
        enabled,
        egui::Button::new(gettext(app.locale, label))
            .wrap()
            .min_size(egui::vec2(36.0, 36.0)),
    );
    if matches!(intent, Intent::AskEnd) {
        ui.ctx()
            .data_mut(|data| data.insert_temp(egui::Id::new("jam-end-trigger"), response.id));
    }
    if response.clicked() {
        enqueue(app, intent);
    }
}

pub(super) fn section(app: &mut App, ui: &mut egui::Ui) {
    if let Some(until) = app.jam.fresh_until
        && until > tokio::time::Instant::now()
    {
        ui.ctx()
            .request_repaint_after(until.saturating_duration_since(tokio::time::Instant::now()));
    }
    theme::text(ui, "Jam", theme::bold(16.0), app.palette.text);
    ui.add_space(8.0);
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
        text(
            app,
            ui,
            "Wait for this operation to finish. Closing Connect does not cancel it.",
        );
    }
    if let Some(notice) = app.jam.notice {
        text(app, ui, notice);
    }
    if app.jam.error.is_some() {
        text(
            app,
            ui,
            "We could not confirm the Jam state. Check it before trying again.",
        );
    }
    match app.jam.membership.clone() {
        Membership::Unknown => {
            text(app, ui, "Jam state is unconfirmed");
            text(
                app,
                ui,
                "Check whether you already participate before creating a Jam.",
            );
        }
        Membership::Absent => {
            text(app, ui, "Listen and choose together");
            text(
                app,
                ui,
                if app.jam.fresh() {
                    "You are not participating in a Jam."
                } else {
                    "At the last check, you were not participating in a Jam."
                },
            );
            if app.local.connected && app.local.is_active() {
                text(app, ui, "Audio stays on this computer.");
            } else {
                text(app, ui, "Play on this computer before creating a Jam.");
            }
            button(
                app,
                ui,
                "Create Jam",
                app.jam.can_create() && app.local.connected && app.local.is_active(),
                Intent::Create,
            );
            text(
                app,
                ui,
                "Joining with an invitation is not available yet. Use Spotify to join.",
            );
        }
        Membership::Present(p) => {
            theme::text(
                ui,
                gettext(
                    app.locale,
                    if app.jam.fresh() {
                        "Jam active"
                    } else {
                        "Previously active Jam"
                    },
                ),
                theme::bold(14.0),
                app.palette.text,
            );
            text(
                app,
                ui,
                if p.is_session_owner {
                    "Host"
                } else {
                    "Participant"
                },
            );
            if !app.jam.fresh() {
                text(
                    app,
                    ui,
                    "Not updated. Check the Jam state before continuing.",
                );
            }
            if app.jam.fresh() {
                match (p.is_listening, p.is_controlling) {
                    (true, _) => text(app, ui, "Listening here"),
                    (false, true) => text(app, ui, "Controlling the host"),
                    _ => text(app, ui, "Listening mode is unconfirmed"),
                }
            }
            text(app, ui, "Audio state is unconfirmed");
            text(app, ui, "Changing listening mode is not available yet.");
            ui.add_space(8.0);
            text(
                app,
                ui,
                "Invitation unavailable. Use Spotify to invite people.",
            );
            text(app, ui, "Participants are not available here yet.");
            text(
                app,
                ui,
                "Member management and permissions are not available yet.",
            );
            text(app, ui, "Shared queue actions are not available here yet.");
            ui.separator();
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
        }
    }
    if !app.jam.fresh()
        && app.jam.pending.is_none()
        && !matches!(app.jam.membership, Membership::Unknown)
    {
        text(app, ui, "Check the Jam state to continue.");
    }
    ui.add_space(8.0);
    button(
        app,
        ui,
        "Check Jam state",
        app.jam.pending.is_none(),
        Intent::Current,
    );
    ui.add_space(12.0);
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
            "Everyone will stop participating. Stop sharing this invitation.",
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
