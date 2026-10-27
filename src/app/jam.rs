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
        let had_generation = self.jam.generation.is_some();
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
        if had_generation && self.jam.generation.is_none() {
            self.backend.cancel_jam_generation();
        }
        let context_valid = self.backend.jam_context_valid(live);
        if !self.jam.fresh() || !context_valid {
            self.jam.clear_metadata();
        }
        let comparison_visible = self.lyrics_fullscreen.is_none()
            && ((self.show_queue_panel && self.queue_tab == QueueTab::Queue)
                || matches!(self.page(), Page::Queue));
        self.sync_resume_view(ctx);
        if !comparison_visible {
            self.jam.official_open = false;
        }
        if !comparison_visible || !context_valid {
            self.jam.official = None;
            self.backend.cancel_jam_comparison();
        }
        if self.jam.owner_id().is_none() {
            self.jam.official = None;
        }
        // All invalidations must finish before native event copies lose their owner.
        if self.jam.official.is_none() {
            clear_official_focus(ctx);
        }
        self.jam.diagnostic = self.backend.jam_diagnostic();
        self.sync_generation_view(ctx);
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

    fn resume_surface_visible(&self, ctx: &egui::Context) -> bool {
        self.lyrics_fullscreen.is_none()
            && ((self.show_queue_panel && self.queue_tab == QueueTab::Queue)
                || matches!(self.page(), Page::Queue))
            && !self.window_hidden
            && !self.hide_intent
            && !self.quit_requested
            && !self.switch_intent
            && !self.settings.winamp_window
            && !self.show_devices
            && !ctx.input(|i| {
                i.viewport().minimized.unwrap_or(false) || i.viewport().close_requested()
            })
    }

    pub(super) fn sync_resume_view(&self, ctx: &egui::Context) {
        if !self.resume_surface_visible(ctx) {
            self.backend.dismiss_jam_resume();
        } else if !self.backend.jam_context_valid(self.backend.jam_stamp()) {
            self.backend.retire_jam_resume();
        }
    }

    pub(super) fn cancel_generation_view(&mut self) {
        if self.jam.generation.is_some() {
            self.jam.metadata.invitation = None;
            self.jam.close_invitation();
        }
        self.jam.generation = None;
        self.jam.generation_ready = false;
        self.backend.cancel_jam_generation();
    }

    /// Read-only backend checks and local retirement; never sends or refreshes.
    pub(super) fn sync_generation_view(&mut self, ctx: &egui::Context) {
        let live = self.backend.jam_stamp();
        let visible = !self.window_hidden
            && !self.hide_intent
            && !self.quit_requested
            && !self.switch_intent
            && !self.settings.winamp_window
            && !self.show_devices
            && self.lyrics_fullscreen.is_none()
            && !ctx.input(|input| {
                input.viewport().minimized.unwrap_or(false) || input.viewport().close_requested()
            })
            && ((self.show_queue_panel && self.queue_tab == QueueTab::Queue)
                || matches!(self.page(), Page::Queue));
        let host = self.jam.owner_id().is_some() && self.backend.jam_context_valid(live);
        let current = self.jam.generation.as_ref().is_none_or(|view| {
            view.expected.account == live.account
                && view.expected.engine == live.engine
                && view.expected.revision == live.revision
                && (view.expected.operation == live.operation
                    || (view.sent && live.operation == view.expected.operation.wrapping_add(1)))
        });
        // A queued start event can race poll_jam/stamp. Retain only the readonly
        // progress surface within its original TTL; this grants no membership
        // or confirmation capability while the model awaits that event.
        let progress = self.jam.generation.as_ref().is_some_and(|view| {
            view.sent
                && tokio::time::Instant::now() < view.until
                && self.backend.jam_context_valid(live)
        });
        if self.jam.generation.is_some() && (!visible || (!host && !progress) || !current) {
            self.cancel_generation_view();
        }
        self.jam.generation_ready = visible && host && self.backend.jam_generation_available(live);
        if let Some(view) = &mut self.jam.generation
            && view.sent
        {
            view.report = self.backend.jam_generation_report();
            let invitation = self.backend.jam_generation_invitation(live);
            if invitation.is_none() {
                self.jam.metadata.invitation = None;
                self.jam.close_invitation();
            } else if self.jam.metadata.invitation.is_none() {
                self.jam.metadata.invitation = invitation;
                if let Some(url) = self.jam.invitation() {
                    self.jam.qr =
                        qrcodegen::QrCode::encode_text(url, qrcodegen::QrCodeEcc::Medium).ok();
                    self.jam.invitation_open = true;
                    ctx.data_mut(|data| {
                        data.insert_temp(egui::Id::new("jam-invitation-focus"), true)
                    });
                }
            }
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
            Intent::EditOfficial(value) | Intent::PasteOfficial(value) => value.take(),
            _ => None,
        };
        self.consume_jam(ctx);
        let intent = action.intent;
        if !matches!(
            intent,
            Intent::AskGeneration
                | Intent::ConfirmGeneration
                | Intent::CancelGeneration
                | Intent::OpenInvitation
                | Intent::CopyInvitation
                | Intent::ExpandQr
                | Intent::CloseQr
        ) && self.jam.generation.is_some()
        {
            self.cancel_generation_view();
        }
        if !matches!(
            intent,
            Intent::AskCompare
                | Intent::EditOfficial(_)
                | Intent::PasteOfficial(_)
                | Intent::BackspaceOfficial
                | Intent::ClearOfficial
                | Intent::CompareOfficial
                | Intent::CancelCompare
        ) {
            self.jam.official_open = false;
            self.jam.official = None;
            self.backend.cancel_jam_comparison();
        }
        if !matches!(
            intent,
            Intent::Close
                | Intent::CancelEnd
                | Intent::CancelJoin
                | Intent::CancelCompare
                | Intent::CancelGeneration
                | Intent::CancelResume
        ) && action.expected != self.jam.stamp
        {
            return;
        }
        match intent {
            Intent::Resume(gesture) => {
                if !self.resume_surface_visible(ctx) {
                    return;
                }
                if let Some(request) = self.backend.capture_jam_resume(action.expected, gesture) {
                    self.backend.send(Command::JamResume(request));
                }
                return;
            }
            Intent::CancelResume => {
                self.backend.cancel_jam_resume();
                return;
            }
            Intent::AskGeneration => {
                if self.jam.generation.is_none() && self.jam.generation_ready {
                    let Some(until) = self.jam.fresh_until else {
                        return;
                    };
                    self.jam.generation = Some(crate::model::jam::GenerationView {
                        expected: self.jam.stamp,
                        until,
                        sent: false,
                        report: None,
                    });
                }
                return;
            }
            Intent::ConfirmGeneration => {
                if !self.jam.generation_ready
                    || !self.backend.jam_generation_available(action.expected)
                {
                    return;
                }
                let Some(view) = &mut self.jam.generation else {
                    return;
                };
                if view.sent || view.expected != action.expected {
                    return;
                }
                view.sent = true;
                view.report = Some(crate::backend::jam::generation::Terminal::Waiting);
                self.backend.send(Command::JamGenerate(
                    crate::backend::jam::generation::Intent::explicitly_authorized(action.expected),
                ));
                self.jam.generation_ready = false;
                return;
            }
            Intent::CancelGeneration => {
                self.cancel_generation_view();
                return;
            }
            Intent::AskCompare => {
                if self.jam.owner_id().is_some() {
                    self.backend.cancel_jam_comparison();
                    self.jam.official_open = true;
                    self.jam.official = Some(Default::default());
                }
                return;
            }
            Intent::EditOfficial(_) | Intent::PasteOfficial(_) => {
                if self.jam.owner_id().is_some()
                    && let Some(draft) = &mut self.jam.official
                    && let Some(value) = edit
                {
                    if matches!(intent, Intent::PasteOfficial(_)) {
                        draft.clear();
                    }
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
                self.jam.official_open = false;
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
                if self.jam.invitation().is_none() {
                    ctx.data_mut(|data| {
                        data.insert_temp(egui::Id::new("jam-management-focus"), true)
                    });
                }
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
                if jam.generation.is_some() {
                    if jam.invitation_open && jam.invitation().is_some() {
                        let _ =
                            self.backend
                                .with_jam_generation_invitation(action.expected, |link| {
                                    ctx.copy_text(link.url().to_owned());
                                    jam.notice =
                                        Some("Copy requested. Clipboard delivery is unconfirmed.");
                                });
                    }
                    return;
                }
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
