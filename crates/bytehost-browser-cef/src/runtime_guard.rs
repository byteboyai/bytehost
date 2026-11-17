//! Pure preconditions shared by the GUI runtime and tests (no CEF SDK needed).
use bytehost_browser_core::{BrowserError, ErrorClass, TargetIdentity};

pub(crate) fn validate_identity(
    actual: &TargetIdentity,
    requested: &TargetIdentity,
) -> Result<(), BrowserError> {
    if actual != requested {
        return Err(BrowserError::new(
            ErrorClass::TargetUnavailable,
            "target identity is stale or belongs to another provider/scope/session",
        )
        .with_target(requested.clone()));
    }
    Ok(())
}

pub(crate) fn require_single_target(live: usize, pending: bool) -> Result<(), BrowserError> {
    if live != 0 || pending {
        return Err(BrowserError::new(
            ErrorClass::UnsupportedCapability,
            "this runtime supports one live target; close it before creating another",
        ));
    }
    Ok(())
}

pub(crate) fn require_closed(live: usize, pending: bool) -> Result<(), BrowserError> {
    if live != 0 || pending {
        return Err(BrowserError::new(
            ErrorClass::Timeout,
            "browser close did not complete; CEF shutdown skipped; host must terminate the process",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytehost_browser_core::*;

    fn identity() -> TargetIdentity {
        TargetIdentity::new(
            ProviderVersion::new("cef-156.0.30").unwrap(),
            ProfileScope::project("project-a").unwrap(),
            BrowserSessionId::new("session-current").unwrap(),
            TargetId::new("target-1").unwrap(),
            TargetGeneration::new(2),
        )
    }

    #[test]
    fn full_identity_required_even_when_target_id_is_reused() {
        let actual = identity();
        let mut cases = vec![actual.clone(); 5];
        cases[0].provider = ProviderVersion::new("other-provider").unwrap();
        cases[1].scope = ProfileScope::Global;
        cases[2].session = BrowserSessionId::new("session-old").unwrap();
        cases[3].generation = TargetGeneration::new(1);
        cases[4].target = TargetId::new("target-2").unwrap();
        for stale in cases {
            assert_eq!(
                validate_identity(&actual, &stale).unwrap_err().class,
                ErrorClass::TargetUnavailable
            );
        }
        assert!(validate_identity(&actual, &actual).is_ok());
    }

    #[test]
    fn second_target_is_rejected_during_and_after_creation() {
        assert!(require_single_target(0, false).is_ok());
        for (live, pending) in [(0, true), (1, false), (1, true), (2, false)] {
            assert_eq!(
                require_single_target(live, pending).unwrap_err().class,
                ErrorClass::UnsupportedCapability
            );
        }
        // After OnBeforeClose removed the old target, creation is available.
        assert!(require_single_target(0, false).is_ok());
    }

    #[test]
    fn close_timeout_cannot_be_reported_as_success() {
        assert!(require_closed(0, false).is_ok());
        for (live, pending) in [(1, false), (0, true), (1, true)] {
            assert_eq!(
                require_closed(live, pending).unwrap_err().class,
                ErrorClass::Timeout
            );
        }
    }
}
