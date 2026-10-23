//! Apply deliberate Jam intentions only after the view has drawn.
use super::*;
use crate::backend::jam::{
    CreateIntent, EndIntent, JoinIntent, Link,
    lifecycle::{self, Kind},
};
use crate::model::jam::{Confirmation, Intent};

impl App {
    pub(super) fn consume_jam(&mut self, ctx: &egui::Context) {
        let had_confirmation = self.jam.confirmation.is_some();
        let had_join = self.jam.join.is_some();
        let had_invitation = self.jam.invitation_open;
        let event = self.backend.poll_jam();
        let live = self.backend.jam_stamp();
        if let Some(event) = event
            && self.jam.observe(&event, live)
        {
            if matches!(
                event.phase,
                lifecycle::Phase::Complete(Kind::Create | Kind::Join)
            ) && event.snapshot.error.is_none()
                && self.jam.fresh()
                && matches!(
                    self.jam.membership,
                    crate::backend::jam::Membership::Present(_)
                )
            {
                self.open_jam_management(ctx);
            }
            if event.snapshot.error.is_some()
                && !self.show_devices
                && !(self.show_queue_panel && self.queue_tab == QueueTab::Queue)
                && !matches!(self.page(), Page::Queue)
            {
                self.toast(gettext(
                    self.locale,
                    "The Jam needs attention. Open the queue to check its state.",
                ));
            }
        }
        self.jam.sync(live);
        let comparison_visible = (self.show_queue_panel && self.queue_tab == QueueTab::Queue)
            || matches!(self.page(), Page::Queue);
        if !comparison_visible {
            self.jam.official = None;
            self.backend.cancel_jam_comparison();
        }
        if self.jam.owner_id().is_none() {
            self.jam.official = None;
        }
        if self.jam.official.is_none() {
            clear_official_focus(ctx);
        }
        self.jam.diagnostic = self.backend.jam_diagnostic();
        if !self.jam.fresh() || !self.backend.jam_context_valid(live) {
            self.jam.clear_metadata();
        }
        if had_invitation && !self.jam.invitation_open {
            ctx.data_mut(|data| data.insert_temp(egui::Id::new("jam-management-focus"), true));
        }
        if !self.jam.can_create() || !self.local.connected {
            self.jam.join = None;
        }
        if self.jam.join.is_none() {
            // egui's undo history also contains private draft strings.
            ctx.data_mut(|data| {
                data.remove::<egui::text_edit::TextEditState>(egui::Id::new("jam-join-link"))
            });
            if had_join && self.jam.pending.is_none() {
                self.jam.notice = Some("The Jam changed. Check its state before continuing.");
                ctx.data_mut(|data| data.insert_temp(egui::Id::new("jam-management-focus"), true));
            }
        }
        if had_confirmation && (!self.jam.fresh() || self.jam.confirmation.is_none()) {
            self.jam.confirmation = None;
            self.jam.notice = Some("The Jam changed. Check its state before continuing.");
            ctx.data_mut(|data| data.insert_temp(egui::Id::new("jam-management-focus"), true));
        }
    }

    fn open_jam_management(&mut self, ctx: &egui::Context) {
        self.leave_lyrics_fullscreen(ctx);
        self.show_devices = false;
        self.show_lyrics_panel = false;
        self.queue_tab = QueueTab::Queue;
        self.show_queue_panel = !matches!(self.page(), Page::Queue);
        ctx.data_mut(|data| data.insert_temp(egui::Id::new("jam-management-focus"), true));
    }

    pub(super) fn jam_blocks_transfer(&self) -> bool {
        self.backend.jam_write_uncertain() || (self.jam.engaged && self.jam.blocks_transfer())
    }

    pub(super) fn apply_jam(&mut self, action: crate::model::jam::UiAction, ctx: &egui::Context) {
        // Drain edits even when a stale intention is refused; clones hold no replayable text.
        let edit = match &action.intent {
            Intent::EditOfficial(value) => value.take(),
            _ => None,
        };
        self.consume_jam(ctx);
        let intent = action.intent;
        if !matches!(
            intent,
            Intent::AskCompare
                | Intent::EditOfficial(_)
                | Intent::BackspaceOfficial
                | Intent::ClearOfficial
                | Intent::CompareOfficial
                | Intent::CancelCompare
        ) {
            self.jam.official = None;
            self.backend.cancel_jam_comparison();
        }
        if !matches!(
            intent,
            Intent::Close | Intent::CancelEnd | Intent::CancelJoin | Intent::CancelCompare
        ) && action.expected != self.jam.stamp
        {
            return;
        }
        match intent {
            Intent::AskCompare => {
                if self.jam.owner_id().is_some() {
                    self.backend.cancel_jam_comparison();
                    self.jam.official = Some(Default::default());
                }
                return;
            }
            Intent::EditOfficial(_) => {
                if self.jam.owner_id().is_some()
                    && let Some(draft) = &mut self.jam.official
                    && let Some(value) = edit
                {
                    draft.append(value);
                }
                return;
            }
            Intent::BackspaceOfficial => {
                if let Some(draft) = &mut self.jam.official {
                    draft.backspace();
                }
                return;
            }
            Intent::ClearOfficial => {
                if let Some(draft) = &mut self.jam.official {
                    draft.clear();
                }
                return;
            }
            Intent::CancelCompare => {
                self.jam.official = None;
                clear_official_focus(ctx);
                self.backend.cancel_jam_comparison();
                self.jam.diagnostic = self.backend.jam_diagnostic();
                return;
            }
            Intent::OpenInvitation => {
                if let Some(url) = self.jam.invitation() {
                    let qr = qrcodegen::QrCode::encode_text(url, qrcodegen::QrCodeEcc::Medium).ok();
                    self.jam.qr = qr;
                    self.jam.invitation_open = true;
                    ctx.data_mut(|data| {
                        data.insert_temp(egui::Id::new("jam-invitation-focus"), true)
                    });
                }
                return;
            }
            Intent::CloseInvitation => {
                self.jam.close_invitation();
                ctx.data_mut(|data| data.insert_temp(egui::Id::new("jam-invitation-return"), true));
                return;
            }
            Intent::ExpandQr => {
                if self.jam.invitation().is_some()
                    && self.jam.invitation_open
                    && self.jam.qr.is_some()
                {
                    self.jam.qr_expanded = true;
                }
                return;
            }
            Intent::CloseQr => {
                self.jam.qr_expanded = false;
                ctx.data_mut(|data| data.insert_temp(egui::Id::new("jam-qr-return"), true));
                return;
            }
            Intent::CopyInvitation => {
                let jam = &mut self.jam;
                let _ = self.backend.with_jam_context(action.expected, || {
                    if let Some(url) = jam.invitation().filter(|_| jam.invitation_open) {
                        ctx.copy_text(url.to_owned());
                        jam.notice = Some("Copy requested. Clipboard delivery is unconfirmed.");
                    }
                });
                return;
            }
            Intent::Manage => {
                self.open_jam_management(ctx);
                return;
            }
            Intent::Close => {
                self.show_devices = false;
                self.jam.confirmation = None;
                self.jam.join = None;
                if let Some(id) =
                    ctx.data(|data| data.get_temp::<egui::Id>(egui::Id::new("connect-button-id")))
                {
                    ctx.memory_mut(|memory| memory.request_focus(id));
                }
                return;
            }
            Intent::CancelEnd => {
                if let Some(confirmation) = self.jam.confirmation.take()
                    && confirmation.stamp == self.jam.stamp
                    && self.jam.owner_id() == Some(confirmation.session_id.as_str())
                {
                    // Restore after the modal layer is removed on the next draw.
                    ctx.data_mut(|data| {
                        data.insert_temp(egui::Id::new("jam-return-focus"), self.jam.stamp)
                    });
                }
                return;
            }
            Intent::AskEnd => {
                if let Some(id) = self.jam.owner_id() {
                    self.jam.confirmation = Some(Confirmation {
                        stamp: self.jam.stamp,
                        session_id: id.to_owned(),
                    });
                }
                return;
            }
            Intent::AskJoin => {
                if self.jam.can_create() && self.local.connected {
                    self.jam.join = Some(Default::default());
                    ctx.data_mut(|data| data.insert_temp(egui::Id::new("jam-join-focus"), true));
                }
                return;
            }
            Intent::CancelJoin => {
                self.jam.join = None;
                ctx.data_mut(|data| {
                    data.remove::<egui::text_edit::TextEditState>(egui::Id::new("jam-join-link"));
                    data.insert_temp(egui::Id::new("jam-join-return"), self.jam.stamp);
                });
                return;
            }
            Intent::EditJoin(value) => {
                if let Some(draft) = &mut self.jam.join {
                    draft.link = value.0;
                    draft.invalid = false;
                }
                return;
            }
            Intent::RequestListenHere => {
                if let Some(draft) = &mut self.jam.join {
                    draft.listen_here = true;
                }
                return;
            }
            _ => (),
        }
        if self.jam.pending.is_some() {
            return;
        }
        let (action, kind) = match intent {
            Intent::Current => (lifecycle::Action::Current, Kind::Current),
            Intent::DiagnoseCurrent => (
                lifecycle::Action::DiagnoseCurrent(Default::default()),
                Kind::Current,
            ),
            Intent::CompareOfficial if self.jam.owner_id().is_some() => {
                let Some(draft) = &mut self.jam.official else {
                    return;
                };
                let Ok(link) = draft.take_link() else {
                    return;
                };
                (
                    lifecycle::Action::DiagnoseCurrent(
                        crate::backend::jam::diagnostic::Probe::with_official(link),
                    ),
                    Kind::Current,
                )
            }
            Intent::Create
                if self.jam.can_create() && self.local.connected && self.local.is_active() =>
            {
                (
                    lifecycle::Action::Create(CreateIntent::Requested),
                    Kind::Create,
                )
            }
            Intent::ConfirmJoin if self.jam.can_create() && self.local.connected => {
                let Some(draft) = &mut self.jam.join else {
                    return;
                };
                if !draft.listen_here {
                    return;
                }
                let Ok(link) = Link::parse(&draft.link) else {
                    draft.invalid = true;
                    ctx.data_mut(|data| data.insert_temp(egui::Id::new("jam-join-focus"), true));
                    return;
                };
                (
                    lifecycle::Action::Join {
                        link,
                        intent: JoinIntent::ListenAndControlRequested,
                    },
                    Kind::Join,
                )
            }
            Intent::ConfirmEnd => {
                let Some(confirmation) = self.jam.confirmation.take() else {
                    return;
                };
                if confirmation.stamp != self.jam.stamp
                    || self.jam.owner_id() != Some(confirmation.session_id.as_str())
                {
                    return;
                }
                (
                    lifecycle::Action::End {
                        session_id: confirmation.session_id,
                        intent: EndIntent::ForEveryone,
                    },
                    Kind::End,
                )
            }
            _ => return,
        };
        let expected = self.jam.stamp;
        self.jam.start(kind);
        clear_official_focus(ctx);
        ctx.data_mut(|data| {
            data.remove::<egui::text_edit::TextEditState>(egui::Id::new("jam-join-link"))
        });
        self.backend
            .send(Command::Jam(lifecycle::Request { expected, action }));
    }
}

// Discard private native events if the capture disappeared before this frame drew.
fn clear_official_focus(ctx: &egui::Context) {
    let id = ctx.data_mut(|data| {
        let key = egui::Id::new("jam-official-focus-id");
        let id = data.get_temp::<egui::Id>(key);
        data.remove::<egui::Id>(key);
        id
    });
    if let Some(id) = id
        && ctx.memory(|memory| memory.has_focus(id))
    {
        ctx.input_mut(|input| {
            let public = |event: &egui::Event| {
                !matches!(
                    event,
                    egui::Event::Text(_)
                        | egui::Event::Paste(_)
                        | egui::Event::Copy
                        | egui::Event::Cut
                )
            };
            input.events.retain(public);
            input.raw.events.retain(public);
        });
        ctx.memory_mut(|memory| memory.surrender_focus(id));
    }
}

#[cfg(test)]
mod official_tests {
    use super::*;
    #[test]
    fn official_capture_close_purges_both_native_event_copies_and_focus() {
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let response = ui.button("hidden capture");
            response.request_focus();
            ctx.data_mut(|data| {
                data.insert_temp(egui::Id::new("jam-official-focus-id"), response.id)
            });
        });
        output.textures_delta.clear();
        let mut output =
            ctx.run_ui(
                egui::RawInput {
                    events: vec![egui::Event::Paste("SyntheticSecret9".into())],
                    ..Default::default()
                },
                |ui| {
                    clear_official_focus(ui.ctx());
                    ui.ctx().input(|input| {
                        assert!(!input.events.iter().chain(input.raw.events.iter()).any(
                            |event| matches!(event, egui::Event::Paste(_) | egui::Event::Text(_))
                        ))
                    });
                    assert!(ui.ctx().memory(|memory| memory.focused().is_none()));
                },
            );
        output.textures_delta.clear();
    }
}
