use std::{
    mem::size_of,
    sync::{mpsc, Arc, Mutex, OnceLock},
    thread,
    time::Duration,
};

use windows::{
    core::{factory, w, HSTRING, PCWSTR, PWSTR},
    Foundation::{AsyncOperationCompletedHandler, AsyncStatus, IAsyncOperation},
    Security::Credentials::UI::{
        UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
    },
    Win32::{
        Foundation::{CloseHandle, ERROR_CANCELLED, ERROR_SUCCESS, HANDLE, HWND},
        Security::{
            Credentials::{
                CredUIPromptForWindowsCredentialsW, CredUnPackAuthenticationBufferW,
                CREDUIWIN_ENUMERATE_CURRENT_USER, CREDUIWIN_SECURE_PROMPT, CREDUI_INFOW,
                CRED_PACK_PROTECTED_CREDENTIALS,
            },
            EqualSid, GetTokenInformation, LogonUserW, TokenUser, LOGON32_LOGON_INTERACTIVE,
            LOGON32_PROVIDER_DEFAULT, TOKEN_QUERY, TOKEN_USER,
        },
        System::{
            Com::CoTaskMemFree,
            StationsAndDesktops::{CloseDesktop, OpenInputDesktop, DESKTOP_READOBJECTS},
            Threading::{GetCurrentProcess, OpenProcessToken},
            WinRT::{
                IUserConsentVerifierInterop, RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED,
            },
        },
        UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, TranslateMessage, MSG,
            PM_REMOVE, WINDOW_EX_STYLE, WS_OVERLAPPED,
        },
    },
};
use zeroize::Zeroize;

use crate::{no_presence, Outcome, Withdraw};

// Leave margin for the 5 s CI assertion, including thread startup. Even a
// stalled COM activation cannot hold the caller past this receive deadline.
const PRECHECK_LIMIT: Duration = Duration::from_secs(4);

#[derive(Debug)]
struct Precheck {
    hello: bool,
    input_desktop: bool,
}

struct Apartment;
impl Apartment {
    #[allow(unsafe_code)]
    fn new() -> windows::core::Result<Self> {
        // SAFETY: this blocking worker uses MTA WinRT and balances each
        // successful initialization on the same thread in Drop.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
        Ok(Self)
    }
}
impl Drop for Apartment {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: paired with this thread's successful RoInitialize.
        unsafe { RoUninitialize() };
    }
}

#[allow(unsafe_code)]
fn probe_input_desktop() -> bool {
    // SAFETY: request only read access; a successful handle is owned and closed
    // here. No desktop switch and no window is performed by the pre-check.
    unsafe {
        match OpenInputDesktop(Default::default(), false, DESKTOP_READOBJECTS) {
            Ok(desktop) => {
                let _ = CloseDesktop(desktop);
                true
            }
            Err(_) => false,
        }
    }
}

fn real_precheck() -> Result<Precheck, ()> {
    // A COM server can stall even during activation. Bound both the caller's
    // wait and the number of probe threads: reuse one worker with a one-entry
    // queue, rather than leaking another thread on every timed-out request.
    type Reply = mpsc::SyncSender<Precheck>;
    static WORKER: OnceLock<Result<mpsc::SyncSender<Reply>, ()>> = OnceLock::new();
    let worker = WORKER.get_or_init(|| {
        let (requests, receiver) = mpsc::sync_channel::<Reply>(1);
        thread::Builder::new()
            .name("subc-presence-precheck".into())
            .spawn(move || {
                while let Ok(reply) = receiver.recv() {
                    let input_desktop = probe_input_desktop();
                    let hello = Apartment::new()
                        .and_then(|_apartment| UserConsentVerifier::CheckAvailabilityAsync()?.get())
                        .is_ok_and(|availability| {
                            availability == UserConsentVerifierAvailability::Available
                        });
                    let _ = reply.send(Precheck {
                        hello,
                        input_desktop,
                    });
                }
            })
            .map(|_| requests)
            .map_err(|_| ())
    });
    let (sender, receiver) = mpsc::sync_channel(1);
    worker
        .as_ref()
        .map_err(|_| ())?
        .try_send(sender)
        .map_err(|_| ())?;
    receiver.recv_timeout(PRECHECK_LIMIT).map_err(|_| ())
}

#[derive(Default)]
struct State {
    withdrawn: bool,
    // HWND is deliberately stored as an integer, never dereferenced in Rust.
    // It is used only while the owner thread is alive, under this mutex.
    window: Option<usize>,
    operation: Option<IAsyncOperation<UserConsentVerificationResult>>,
}

struct Handle {
    state: Arc<Mutex<State>>,
    close: mpsc::Sender<()>,
}
impl Withdraw for Handle {
    fn withdraw(&self) {
        let operation = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.withdrawn = true;
            state.operation.clone()
        };
        // Cancel the Hello operation as well as destroying the owning window.
        // Destruction is dispatched to its creating thread: DestroyWindow on a
        // different thread is expressly not supported by Win32.
        let _ = self.close.send(());
        if let Some(operation) = operation {
            let _ = operation.Cancel();
        }
    }
}

struct Owner {
    close: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Owner {
    #[allow(unsafe_code)]
    fn new(state: Arc<Mutex<State>>) -> Result<Self, ()> {
        let (close, requests) = mpsc::channel();
        let (ready, initialized) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("subc-presence-window".into())
            .spawn(move || {
                // SAFETY: STATIC is a system-registered class. This hidden
                // top-level window belongs to this thread, which pumps its
                // messages and destroys it before exiting. No Rust pointers
                // are installed in a window procedure or passed to Win32.
                let window = unsafe {
                    CreateWindowExW(
                        WINDOW_EX_STYLE::default(),
                        w!("STATIC"),
                        w!("subc operator confirmation"),
                        WS_OVERLAPPED,
                        0,
                        0,
                        0,
                        0,
                        None,
                        None,
                        None,
                        None,
                    )
                };
                let Ok(window) = window else {
                    let _ = ready.send(Err(()));
                    return;
                };
                state.lock().unwrap_or_else(|e| e.into_inner()).window = Some(window.0 as usize);
                let _ = ready.send(Ok(()));
                loop {
                    if !matches!(requests.try_recv(), Err(mpsc::TryRecvError::Empty)) {
                        break;
                    }
                    let mut message = MSG::default();
                    // SAFETY: message is a valid writable MSG; the pump is on
                    // the window's owning thread and never reads Rust userdata.
                    unsafe {
                        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                            let _ = TranslateMessage(&message);
                            DispatchMessageW(&message);
                        }
                    }
                    if requests.recv_timeout(Duration::from_millis(10)).is_ok() {
                        break;
                    }
                }
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                state.window = None;
                // SAFETY: destroy on the creating thread while holding the
                // lock that prevents a concurrent Hello launch using this HWND.
                unsafe {
                    let _ = DestroyWindow(window);
                }
            })
            .map_err(|_| ())?;
        match initialized.recv() {
            Ok(Ok(())) => Ok(Self {
                close,
                thread: Some(worker),
            }),
            _ => {
                let _ = worker.join();
                Err(())
            }
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.close.send(());
        if let Some(worker) = self.thread.take() {
            let _ = worker.join();
        }
    }
}

fn hello_outcome(result: UserConsentVerificationResult) -> Outcome {
    match result {
        UserConsentVerificationResult::Verified => Outcome::Approved,
        UserConsentVerificationResult::Canceled => Outcome::Declined,
        _ => Outcome::ProviderError,
    }
}

#[allow(unsafe_code)]
fn hello(text: &str, state: &Mutex<State>) -> Outcome {
    let Ok(interop) = factory::<UserConsentVerifier, IUserConsentVerifierInterop>() else {
        return Outcome::ProviderError;
    };
    let operation = {
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        if state.withdrawn {
            return Outcome::ProviderError;
        }
        let Some(window) = state.window else {
            return Outcome::ProviderError;
        };
        // SAFETY: the owner thread keeps the HWND alive under this lock. The
        // message is an owned HSTRING and the requested WinRT interface is typed.
        let operation: windows::core::Result<IAsyncOperation<UserConsentVerificationResult>> = unsafe {
            interop.RequestVerificationForWindowAsync(HWND(window as *mut _), &HSTRING::from(text))
        };
        let Ok(operation) = operation else {
            return Outcome::ProviderError;
        };
        state.operation = Some(operation.clone());
        operation
    };
    let (sender, receiver) = mpsc::sync_channel(1);
    if operation
        .SetCompleted(&AsyncOperationCompletedHandler::new(move |_, _| {
            let _ = sender.send(());
            Ok(())
        }))
        .is_err()
    {
        let _ = operation.Cancel();
        // A failed callback registration does not prove that the dialog ended.
        // Wait for the actual operation to stop; an unqueryable operation stays
        // stuck rather than releasing the daemon's slot on a guess.
        while !operation
            .Status()
            .is_ok_and(|status| status != AsyncStatus::Started)
        {
            thread::sleep(Duration::from_millis(10));
        }
    } else if receiver.recv().is_err() {
        return Outcome::ProviderError;
    }
    // Do not return merely because withdraw was called. The daemon's global
    // prompt slot must stay occupied until Windows actually ends the operation.
    let result = operation
        .GetResults()
        .map(hello_outcome)
        .unwrap_or(Outcome::ProviderError);
    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
    state.operation = None;
    if state.withdrawn {
        Outcome::ProviderError
    } else {
        result
    }
}

struct Token(HANDLE);
impl Drop for Token {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        // SAFETY: this wrapper is constructed only for an owned successful token.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[allow(unsafe_code)]
fn token_user(token: &Token) -> windows::core::Result<Vec<usize>> {
    let mut bytes = 0;
    // SAFETY: the initial call queries the size without a writable buffer.
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut bytes) };
    if bytes < size_of::<TOKEN_USER>() as u32 || bytes > 65536 {
        return Err(windows::core::Error::from_win32());
    }
    // A word buffer provides TOKEN_USER's pointer alignment as well as room for
    // the embedded SID. It stays alive until EqualSid has read both SIDs.
    let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
    // SAFETY: the aligned allocation holds at least bytes writable bytes.
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            bytes,
            &mut bytes,
        )?;
    }
    Ok(buffer)
}

#[allow(unsafe_code)]
fn same_user(token: &Token) -> windows::core::Result<bool> {
    let mut current = HANDLE::default();
    // SAFETY: query the current process token, never impersonate the supplied
    // identity. Each successful handle is owned by a Token and promptly closed.
    unsafe {
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut current)?;
    }
    let current = Token(current);
    let supplied = token_user(token)?;
    let logged_on = token_user(&current)?;
    // SAFETY: both aligned buffers contain validated TOKEN_USER records and
    // their referenced SIDs remain allocated throughout EqualSid.
    unsafe {
        let supplied = &*supplied.as_ptr().cast::<TOKEN_USER>();
        let logged_on = &*logged_on.as_ptr().cast::<TOKEN_USER>();
        Ok(EqualSid(supplied.User.Sid, logged_on.User.Sid).is_ok())
    }
}

struct AuthBuffer {
    pointer: *mut std::ffi::c_void,
    bytes: u32,
}
impl Drop for AuthBuffer {
    #[allow(unsafe_code)]
    fn drop(&mut self) {
        if self.pointer.is_null() {
            return;
        }
        // SAFETY: CredUI allocated exactly bytes writable bytes with COM's
        // allocator. Volatile stores prevent removal of the credential wipe.
        unsafe {
            for index in 0..self.bytes as usize {
                self.pointer.cast::<u8>().add(index).write_volatile(0);
            }
            CoTaskMemFree(Some(self.pointer));
        }
    }
}

struct Secrets {
    user: Vec<u16>,
    domain: Vec<u16>,
    password: Vec<u16>,
}
impl Drop for Secrets {
    fn drop(&mut self) {
        self.user.zeroize();
        self.domain.zeroize();
        self.password.zeroize();
    }
}

#[allow(unsafe_code)]
fn check_credentials(buffer: &AuthBuffer) -> Outcome {
    let (mut user_len, mut domain_len, mut password_len) = (0, 0, 0);
    // SAFETY: query required UTF-16 lengths without providing output buffers.
    let _ = unsafe {
        CredUnPackAuthenticationBufferW(
            CRED_PACK_PROTECTED_CREDENTIALS,
            buffer.pointer,
            buffer.bytes,
            PWSTR::null(),
            &mut user_len,
            PWSTR::null(),
            Some(&mut domain_len),
            PWSTR::null(),
            &mut password_len,
        )
    };
    if [user_len, domain_len, password_len]
        .iter()
        .any(|&len| len == 0 || len > 65536)
    {
        return Outcome::ProviderError;
    }
    let mut secrets = Secrets {
        user: vec![0; user_len as usize],
        domain: vec![0; domain_len as usize],
        password: vec![0; password_len as usize],
    };
    // SAFETY: each buffer has the queried writable capacity, including NUL.
    // The immutable blob is owned for the duration of unpacking.
    if unsafe {
        CredUnPackAuthenticationBufferW(
            CRED_PACK_PROTECTED_CREDENTIALS,
            buffer.pointer,
            buffer.bytes,
            PWSTR(secrets.user.as_mut_ptr()),
            &mut user_len,
            PWSTR(secrets.domain.as_mut_ptr()),
            Some(&mut domain_len),
            PWSTR(secrets.password.as_mut_ptr()),
            &mut password_len,
        )
    }
    .is_err()
    {
        return Outcome::ProviderError;
    }
    let mut token = HANDLE::default();
    // SAFETY: the unpacked strings are NUL-terminated and remain live through
    // LogonUser. No credentials are persisted, logged or used for impersonation.
    if unsafe {
        LogonUserW(
            PCWSTR(secrets.user.as_ptr()),
            PCWSTR(secrets.domain.as_ptr()),
            PCWSTR(secrets.password.as_ptr()),
            LOGON32_LOGON_INTERACTIVE,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    }
    .is_err()
    {
        return Outcome::Declined;
    }
    match same_user(&Token(token)) {
        Ok(true) => Outcome::Approved,
        Ok(false) => Outcome::Declined,
        Err(_) => Outcome::ProviderError,
    }
}

#[allow(unsafe_code)]
fn credentials(text: &str, state: &Mutex<State>) -> Outcome {
    let window = {
        let state = state.lock().unwrap_or_else(|e| e.into_inner());
        if state.withdrawn {
            return Outcome::ProviderError;
        }
        let Some(window) = state.window else {
            return Outcome::ProviderError;
        };
        HWND(window as *mut _)
    };
    let text: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let info = CREDUI_INFOW {
        cbSize: size_of::<CREDUI_INFOW>() as u32,
        hwndParent: window,
        pszMessageText: PCWSTR(text.as_ptr()),
        pszCaptionText: w!("subc operator confirmation"),
        ..Default::default()
    };
    let mut package = 0;
    let mut buffer = AuthBuffer {
        pointer: std::ptr::null_mut(),
        bytes: 0,
    };
    // SAFETY: all input and output storage remains live until this blocking
    // call returns. The daemon-owned parent is destroyed on withdraw, including
    // during a shown fallback. Use the secure desktop, with no save checkbox.
    let result = unsafe {
        CredUIPromptForWindowsCredentialsW(
            Some(&info),
            0,
            &mut package,
            None,
            0,
            &mut buffer.pointer,
            &mut buffer.bytes,
            None,
            CREDUIWIN_SECURE_PROMPT | CREDUIWIN_ENUMERATE_CURRENT_USER,
        )
    };
    if result == ERROR_CANCELLED.0 {
        return Outcome::Declined;
    }
    if result != ERROR_SUCCESS.0 || buffer.pointer.is_null() || buffer.bytes == 0 {
        return Outcome::ProviderError;
    }
    if state.lock().unwrap_or_else(|e| e.into_inner()).withdrawn {
        return Outcome::ProviderError;
    }
    check_credentials(&buffer)
}

pub(super) fn prompt(text: &str, publish: Box<dyn FnOnce(Arc<dyn Withdraw>) + Send>) -> Outcome {
    if text.is_empty() || text.contains('\0') {
        return Outcome::ProviderError;
    }
    let Ok(precheck) = real_precheck() else {
        return Outcome::ProviderError;
    };
    after_precheck(precheck, text, publish)
}

fn after_precheck(
    precheck: Precheck,
    text: &str,
    publish: Box<dyn FnOnce(Arc<dyn Withdraw>) + Send>,
) -> Outcome {
    if no_presence(precheck.hello, precheck.input_desktop) {
        return Outcome::NoPresence;
    }
    let Ok(_apartment) = Apartment::new() else {
        return Outcome::ProviderError;
    };
    let state = Arc::new(Mutex::new(State::default()));
    let Ok(owner) = Owner::new(state.clone()) else {
        return Outcome::ProviderError;
    };
    publish(Arc::new(Handle {
        state: state.clone(),
        close: owner.close.clone(),
    }));
    if precheck.hello {
        hello(text, &state)
    } else {
        credentials(text, &state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn windows_real_precheck_finishes_within_five_seconds() {
        let start = Instant::now();
        let result = real_precheck();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "pre-check took {:?}: {result:?}",
            start.elapsed()
        );
        result.expect("the real OS pre-check must complete, not merely hit its timeout wrapper");
    }

    #[test]
    fn windows_no_presence_returns_before_creating_a_window() {
        assert_eq!(
            after_precheck(
                Precheck {
                    hello: false,
                    input_desktop: false
                },
                "no prompt",
                Box::new(|_| panic!("no_presence must not create a window or withdraw handle"))
            ),
            Outcome::NoPresence
        );
    }

    #[test]
    #[allow(unsafe_code)]
    fn windows_withdraw_destroys_the_owner_on_its_creating_thread() {
        let state = Arc::new(Mutex::new(State::default()));
        let owner = Owner::new(state.clone()).expect("create owner");
        let handle = Handle {
            state: state.clone(),
            close: owner.close.clone(),
        };
        let window = state.lock().unwrap().window.expect("owner has a window");
        handle.withdraw();
        drop(owner);
        let state = state.lock().unwrap();
        assert!(state.withdrawn);
        assert!(state.window.is_none());
        // SAFETY: IsWindow accepts an opaque, possibly stale HWND and does not
        // dereference it. Check the real OS object, not just our cleared field.
        assert!(!unsafe {
            windows::Win32::UI::WindowsAndMessaging::IsWindow(HWND(window as *mut _)).as_bool()
        });
    }

    #[test]
    fn windows_only_verified_hello_approves() {
        assert_eq!(
            hello_outcome(UserConsentVerificationResult::Verified),
            Outcome::Approved
        );
        assert_eq!(
            hello_outcome(UserConsentVerificationResult::Canceled),
            Outcome::Declined
        );
        for result in [
            UserConsentVerificationResult::DeviceNotPresent,
            UserConsentVerificationResult::NotConfiguredForUser,
            UserConsentVerificationResult::DisabledByPolicy,
            UserConsentVerificationResult::DeviceBusy,
            UserConsentVerificationResult::RetriesExhausted,
            UserConsentVerificationResult(123),
        ] {
            assert_eq!(hello_outcome(result), Outcome::ProviderError);
        }
    }
}
