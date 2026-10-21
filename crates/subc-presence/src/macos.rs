use std::sync::{mpsc, Arc};

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_foundation::{NSError, NSString};
use objc2_local_authentication::{LAContext, LAPolicy};

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
                .map(|error| (error.domain().to_string(), error.code()))
        };
        let outcome = crate::local_authentication_outcome(
            success.as_bool(),
            error
                .as_ref()
                .map(|(domain, code)| (domain.as_str(), *code)),
        );
        let _ = sender.send(Event::Returned(outcome));
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
                // waiting for that reply: the daemon shows one prompt at a time
                // and starts the next only after this call returns.
                unsafe { context.invalidate() };
            }
        }
    }
    Outcome::ProviderError
}
