//! The one table of CLI exit codes, shared by `cgpt ask` and `cgpt agent`.
//! It matches `docs/requirements.md` §9; a test pins every row.

use cgpt_bridge_protocol::{BridgeResponse, ErrorCode, CLIENT_TIMEOUT_GRACE_MS};

use crate::transport::{AskOutcome, StopOutcome};

/// Stable, do not renumber. 9 (command failed) is in §9 but never returned:
/// a failing command goes back to the assistant and the loop goes on.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    Ok = 0,
    Generic = 1,
    Usage = 2,
    Setup = 3,
    Transport = 4,
    Tab = 5,
    Dom = 6,
    Protocol = 7,
    TabBusy = 8,
    UserCancelled = 10,
    Internal = 11,
}

impl ExitKind {
    pub fn code(self) -> u8 {
        self as u8
    }

    /// Host error code → exit code. A busy tab (`previous_answer_running`,
    /// `ask_running`) is `tab_busy` (8); an extension built before that code,
    /// or one behind such a host, reports it as `dom_failure` (6). The wire
    /// has no separate code for a signed-out window (`not_logged_in`): it
    /// arrives as `tab_unavailable` (5), and the message tells it apart.
    /// `cancelled` (Stop pressed in the ChatGPT page) is the user's own doing
    /// (10); an extension built before that code reports it as `internal` (11).
    pub fn for_error_code(code: ErrorCode) -> Self {
        match code {
            ErrorCode::BadRequest => ExitKind::Usage,
            ErrorCode::OversizeFrame => ExitKind::Transport,
            ErrorCode::ExtensionUnavailable => ExitKind::Setup,
            ErrorCode::TabUnavailable => ExitKind::Tab,
            ErrorCode::DomFailure => ExitKind::Dom,
            ErrorCode::Timeout => ExitKind::Tab,
            ErrorCode::RateLimited => ExitKind::Generic,
            ErrorCode::Cancelled => ExitKind::UserCancelled,
            ErrorCode::TabBusy => ExitKind::TabBusy,
            ErrorCode::Internal => ExitKind::Internal,
        }
    }

    /// The exit code of one ask round trip.
    pub fn for_outcome(outcome: &AskOutcome) -> Self {
        match outcome {
            AskOutcome::Ok(BridgeResponse::AskResult { .. }) => ExitKind::Ok,
            AskOutcome::Ok(BridgeResponse::Error { code, .. }) => Self::for_error_code(*code),
            AskOutcome::Ok(_) => ExitKind::Protocol,
            AskOutcome::SocketMissing => ExitKind::Setup,
            AskOutcome::SocketIo(_) => ExitKind::Transport,
            AskOutcome::Timeout => ExitKind::Tab,
            // Input too large to send: §9 has no code of its own for it.
            AskOutcome::TooLarge { .. } => ExitKind::Usage,
            AskOutcome::BadResponse(_) => ExitKind::Protocol,
            // Not returned: after its line the CLI ends as killed by
            // Ctrl+C (`interrupt::die`), so a shell loop stops too.
            AskOutcome::Interrupted(_) => ExitKind::UserCancelled,
        }
    }
}

/// The one line Ctrl+C during an ask leaves: whether the answer in ChatGPT
/// was stopped, and when that is not known, so.
pub fn interrupted_message(outcome: &StopOutcome) -> String {
    match outcome {
        StopOutcome::Stopped => "stopped: the answer in ChatGPT was interrupted".to_string(),
        StopOutcome::NotRunning => "not stopped: the extension found no answer of this ask \
             being written in ChatGPT"
            .to_string(),
        StopOutcome::NotSent => "stopped: the prompt was not sent to ChatGPT".to_string(),
        StopOutcome::OtherChat => "not stopped: the tab shows another chat".to_string(),
        StopOutcome::OldHost => "interrupted: the answer in ChatGPT was not stopped: \
             the native host is too old to stop one ask; update cgpt-bridge"
            .to_string(),
        StopOutcome::Unconfirmed(why) => format!(
            "interrupted: the stop was not confirmed ({}); \
             the answer in ChatGPT may still be generating",
            why
        ),
    }
}

/// What the CLI says about a request over the host's frame cap.
pub fn too_large_message(len: usize, cap: usize) -> String {
    format!(
        "the request is too large to send: {} bytes, the limit is {} bytes; nothing was sent \
         (shorten the text)",
        len, cap
    )
}

/// What the CLI says when the host did not answer in time. The CLI waits the
/// ask's own limit plus the host's grace, so the number is the whole wait.
pub fn no_answer_message(timeout_ms: u64) -> String {
    format!(
        "no answer from the host after {}ms ({}ms limit + {}ms for the host)",
        timeout_ms.saturating_add(CLIENT_TIMEOUT_GRACE_MS),
        timeout_ms,
        CLIENT_TIMEOUT_GRACE_MS
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn codes_are_the_documented_numbers() {
        let table = [
            (ExitKind::Ok, 0),
            (ExitKind::Generic, 1),
            (ExitKind::Usage, 2),
            (ExitKind::Setup, 3),
            (ExitKind::Transport, 4),
            (ExitKind::Tab, 5),
            (ExitKind::Dom, 6),
            (ExitKind::Protocol, 7),
            (ExitKind::TabBusy, 8),
            (ExitKind::UserCancelled, 10),
            (ExitKind::Internal, 11),
        ];
        for (kind, want) in table {
            assert_eq!(kind.code(), want, "{:?}", kind);
        }
    }

    #[test]
    fn every_host_error_has_its_row() {
        let table = [
            (ErrorCode::BadRequest, ExitKind::Usage),
            (ErrorCode::OversizeFrame, ExitKind::Transport),
            (ErrorCode::ExtensionUnavailable, ExitKind::Setup),
            (ErrorCode::TabUnavailable, ExitKind::Tab),
            (ErrorCode::DomFailure, ExitKind::Dom),
            (ErrorCode::Timeout, ExitKind::Tab),
            (ErrorCode::RateLimited, ExitKind::Generic),
            (ErrorCode::Cancelled, ExitKind::UserCancelled),
            (ErrorCode::TabBusy, ExitKind::TabBusy),
            (ErrorCode::Internal, ExitKind::Internal),
        ];
        for (code, want) in table {
            assert_eq!(ExitKind::for_error_code(code), want, "{:?}", code);
        }
    }

    #[test]
    fn every_outcome_has_its_row() {
        let ask = BridgeResponse::AskResult {
            id: "a".into(),
            text: "t".into(),
            conversation_url: None,
            warning: None,
        };
        let pong = BridgeResponse::Pong {
            id: None,
            host_version: "x".into(),
            build: None,
            echo: None,
            ts_unix_ms: 0,
        };
        let err = BridgeResponse::Error {
            id: None,
            code: ErrorCode::DomFailure,
            message: "m".into(),
        };
        let table = [
            (AskOutcome::Ok(ask), ExitKind::Ok),
            (AskOutcome::Ok(err), ExitKind::Dom),
            (AskOutcome::Ok(pong), ExitKind::Protocol),
            (AskOutcome::SocketMissing, ExitKind::Setup),
            (
                AskOutcome::SocketIo(io::Error::other("x")),
                ExitKind::Transport,
            ),
            (AskOutcome::Timeout, ExitKind::Tab),
            (
                AskOutcome::TooLarge {
                    len: 2_000_000,
                    cap: 1_048_576,
                },
                ExitKind::Usage,
            ),
            (AskOutcome::BadResponse("x".into()), ExitKind::Protocol),
            (
                AskOutcome::Interrupted(StopOutcome::OldHost),
                ExitKind::UserCancelled,
            ),
        ];
        for (outcome, want) in table {
            assert_eq!(ExitKind::for_outcome(&outcome), want);
        }
    }

    #[test]
    fn a_busy_tab_has_its_own_code_and_a_signed_out_one_keeps_its_wire_code() {
        // The extension sends previous_answer_running and ask_running as
        // tab_busy (to a host older than that code: dom_failure, 6) and
        // not_logged_in as tab_unavailable (extension/src/bridge.ts).
        assert_eq!(ExitKind::for_error_code(ErrorCode::TabBusy).code(), 8);
        assert_eq!(ExitKind::for_error_code(ErrorCode::DomFailure).code(), 6);
        assert_eq!(
            ExitKind::for_error_code(ErrorCode::TabUnavailable).code(),
            5
        );
    }

    #[test]
    fn an_interrupted_ask_says_whether_the_answer_was_stopped() {
        let m = |o| interrupted_message(&o);
        assert_eq!(
            m(StopOutcome::Stopped),
            "stopped: the answer in ChatGPT was interrupted"
        );
        // Only a confirmed stop says "stopped".
        assert!(m(StopOutcome::NotRunning).starts_with("not stopped: "));
        assert_eq!(
            m(StopOutcome::OtherChat),
            "not stopped: the tab shows another chat"
        );
        assert_eq!(
            m(StopOutcome::NotSent),
            "stopped: the prompt was not sent to ChatGPT"
        );
        assert!(m(StopOutcome::OldHost).contains("was not stopped: "));
        assert!(m(StopOutcome::OldHost).contains("update cgpt-bridge"));
        let u = m(StopOutcome::Unconfirmed("no answer in time".into()));
        assert!(
            u.contains("the stop was not confirmed (no answer in time)"),
            "{}",
            u
        );
        for o in [
            StopOutcome::Stopped,
            StopOutcome::NotRunning,
            StopOutcome::NotSent,
            StopOutcome::OtherChat,
            StopOutcome::OldHost,
            StopOutcome::Unconfirmed("x".into()),
        ] {
            assert!(!m(o).contains('\n'), "one line");
        }
    }

    #[test]
    fn too_large_message_names_the_size_and_the_limit() {
        let m = too_large_message(1_048_600, 1_048_576);
        assert!(m.contains("too large"), "{}", m);
        assert!(m.contains("1048600 bytes"), "{}", m);
        assert!(m.contains("limit is 1048576 bytes"), "{}", m);
        assert!(m.contains("nothing was sent"), "{}", m);
        assert!(!m.contains('\n'), "one line");
    }

    #[test]
    fn no_answer_message_counts_the_grace() {
        let m = no_answer_message(5_000);
        assert!(m.contains("15000ms"), "{}", m);
        assert!(m.contains("5000ms limit"), "{}", m);
        assert!(m.contains("10000ms for the host"), "{}", m);
    }
}
