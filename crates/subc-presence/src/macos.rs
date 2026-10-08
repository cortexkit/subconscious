use std::sync::{mpsc, Arc};

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_foundation::{NSError, NSString};
use objc2_local_authentication::{LAContext, LAError, LAErrorDomain, LAPolicy};

use crate::{Outcome, Withdraw};

enum Event {
    Returned(Outcome),
    Withdraw,
}

struct Handle(mpsc::Sender<Event>);
impl Withdraw for Handle {
    fn withdraw(&self) {
        let _ = self.0.send(Event::Withdraw);
    }
}

fn outcome(success: bool, error: Option<(bool, isize)>) -> Outcome {
    match (success, error) {
        (true, None) => Outcome::Approved,
        (false, Some((true, code)))
            if code == LAError::UserCancel.0 || code == LAError::AuthenticationFailed.0 =>
        {
            Outcome::Declined
        }
        _ => Outcome::ProviderError,
    }
}

#[allow(unsafe_code)]
pub(super) fn prompt(text: &str, publish: Box<dyn FnOnce(Arc<dyn Withdraw>) + Send>) -> Outcome {
    if text.is_empty() || text.contains('\0') {
        return Outcome::ProviderError;
    }
    // LAContext stays on this blocking thread. Only channel messages cross
    // threads; no unsafe Send/Sync promise is needed for an Objective-C object.
    // SAFETY: new returns an owned context, retained until its reply completes.
    let context = unsafe { LAContext::new() };
    let (sender, receiver) = mpsc::channel();
    publish(Arc::new(Handle(sender.clone())));
    if matches!(receiver.try_recv(), Ok(Event::Withdraw)) {
        return Outcome::ProviderError;
    }
    let reply = RcBlock::new(move |success: Bool, error: *mut NSError| {
        // SAFETY: LocalAuthentication supplies a nullable NSError that is valid
        // for this callback. Read it here, never send the pointer to another thread.
        let error = unsafe {
            error
                .as_ref()
                .map(|error| (*error.domain() == *LAErrorDomain, error.code()))
        };
        let _ = sender.send(Event::Returned(outcome(success.as_bool(), error)));
    });
    // SAFETY: the nonempty reason and sendable reply block are retained for the
    // entire evaluation. DeviceOwnerAuthentication includes the password fallback.
    unsafe {
        context.evaluatePolicy_localizedReason_reply(
            LAPolicy::DeviceOwnerAuthentication,
            &NSString::from_str(text),
            &reply,
        );
    }
    while let Ok(event) = receiver.recv() {
        match event {
            Event::Returned(result) => return result,
            Event::Withdraw => {
                // SAFETY: this thread owns the live context; invalidate is
                // idempotent and ends evaluation with LAErrorAppCancel. Keep
                // waiting for that reply so the daemon does not reuse the slot early.
                unsafe { context.invalidate() };
            }
        }
    }
    Outcome::ProviderError
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_only_explicit_success_approves() {
        assert_eq!(outcome(true, None), Outcome::Approved);
        assert_eq!(outcome(false, None), Outcome::ProviderError);
        for code in [LAError::AppCancel.0, LAError::SystemCancel.0, -1004, 123] {
            assert_eq!(outcome(false, Some((true, code))), Outcome::ProviderError);
        }
        assert_eq!(outcome(true, Some((true, -2))), Outcome::ProviderError);
        assert_eq!(outcome(false, Some((false, -2))), Outcome::ProviderError);
    }

    #[test]
    fn macos_cancel_and_bad_credentials_decline() {
        for code in [LAError::UserCancel.0, LAError::AuthenticationFailed.0] {
            assert_eq!(outcome(false, Some((true, code))), Outcome::Declined);
        }
    }
}
