//! Apply deliberate Jam intentions only after the view has drawn.
use super::*;
use crate::backend::jam::{
    CreateIntent, EndIntent,
    lifecycle::{self, Kind},
};
use crate::model::jam::{Confirmation, Intent};

impl App {
    pub(super) fn consume_jam(&mut self) {
        let event = self.backend.poll_jam();
        let live = self.backend.jam_stamp();
        if let Some(event) = event
            && self.jam.observe(&event, live)
            && event.snapshot.error.is_some()
            && !self.show_devices
        {
            self.toast(gettext(
                self.locale,
                "The Jam needs attention. Open Connect to check its state.",
            ));
        }
        self.jam.sync(live);
        if !self.jam.fresh() && self.jam.confirmation.take().is_some() {
            self.jam.notice = Some("The Jam changed. Check its state before continuing.");
        }
    }

    pub(super) fn jam_used(&self) -> bool {
        self.jam.engaged
    }

    pub(super) fn apply_jam(&mut self, action: crate::model::jam::UiAction, ctx: &egui::Context) {
        self.consume_jam();
        let intent = action.intent;
        if !matches!(intent, Intent::Close | Intent::CancelEnd) && action.expected != self.jam.stamp
        {
            return;
        }
        match intent {
            Intent::Close => {
                self.show_devices = false;
                self.jam.confirmation = None;
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
            _ => (),
        }
        if self.jam.pending.is_some() {
            return;
        }
        let (action, kind) = match intent {
            Intent::Current => (lifecycle::Action::Current, Kind::Current),
            Intent::Create
                if self.jam.can_create() && self.local.connected && self.local.is_active() =>
            {
                (
                    lifecycle::Action::Create(CreateIntent::Requested),
                    Kind::Create,
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
        self.backend
            .send(Command::Jam(lifecycle::Request { expected, action }));
    }
}
