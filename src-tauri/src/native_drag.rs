//! Drop decisions shared by the Windows OLE source and platform-independent tests.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeDragMode {
    Controller,
    Receiver,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NativeDragAction {
    Continue,
    Drop,
    Cancel,
}

impl NativeDragMode {
    pub(crate) fn for_role(role: &str) -> Self {
        if role == "server" {
            Self::Controller
        } else {
            Self::Receiver
        }
    }

    pub(crate) fn action(
        self,
        cancelled: bool,
        released: bool,
        escape_pressed: bool,
        local_button_down: bool,
    ) -> NativeDragAction {
        if cancelled || escape_pressed {
            NativeDragAction::Cancel
        } else if released || (self == Self::Controller && !local_button_down) {
            NativeDragAction::Drop
        } else {
            NativeDragAction::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_handoff_drops_on_local_release_without_a_remote_signal() {
        let mode = NativeDragMode::for_role("server");
        assert_eq!(
            mode.action(false, false, false, true),
            NativeDragAction::Continue
        );
        assert_eq!(
            mode.action(false, false, false, false),
            NativeDragAction::Drop
        );
    }

    #[test]
    fn receiver_waits_for_remote_drop_when_its_local_button_is_up() {
        let mode = NativeDragMode::for_role("client");
        assert_eq!(
            mode.action(false, false, false, false),
            NativeDragAction::Continue
        );
        assert_eq!(
            mode.action(false, true, false, false),
            NativeDragAction::Drop
        );
    }

    #[test]
    fn cancel_and_escape_win_over_button_release_in_both_directions() {
        for mode in [NativeDragMode::Controller, NativeDragMode::Receiver] {
            assert_eq!(
                mode.action(true, true, false, false),
                NativeDragAction::Cancel
            );
            assert_eq!(
                mode.action(false, true, true, false),
                NativeDragAction::Cancel
            );
        }
    }
}
