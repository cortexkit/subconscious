//! N-API ownership bridge. Validation, close-on-exec, PID checks and helper
//! handoff policy live in subc-os so every SDK uses the same platform checks.
use napi::{bindgen_prelude::Buffer, Error, Result};
use napi_derive::napi;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use subc_os::registration_channel as registration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf},
    sync::{Mutex as AsyncMutex, Notify},
};

#[cfg(unix)]
type Stream = tokio::net::UnixStream;
#[cfg(windows)]
type Stream = tokio::net::windows::named_pipe::NamedPipeClient;
type Halves = (AsyncMutex<ReadHalf<Stream>>, AsyncMutex<WriteHalf<Stream>>);

fn failure(_: impl std::fmt::Display) -> Error {
    Error::from_reason("registration native operation failed")
}

#[napi]
pub fn supervised() -> bool {
    registration::supervised()
}
#[napi]
pub fn registration_refusal() -> Option<String> {
    registration::registration_refusal().map(|r| r.to_string())
}
#[napi]
pub fn take_registration_channel() -> Result<Option<NativeChannel>> {
    Ok(registration::take_registration_channel()
        .map_err(failure)?
        .map(|channel| NativeChannel {
            pending: Mutex::new(Some(channel)),
            halves: Mutex::new(None),
            closed: AtomicBool::new(false),
            stop: Notify::new(),
        }))
}

#[napi]
pub struct NativeChannel {
    pending: Mutex<Option<registration::RegistrationChannel>>,
    halves: Mutex<Option<Arc<Halves>>>,
    closed: AtomicBool,
    stop: Notify,
}
impl NativeChannel {
    fn stream(&self) -> Result<Arc<Halves>> {
        let mut halves = self.halves.lock().map_err(failure)?;
        if self.closed.load(Ordering::Acquire) {
            return Err(failure("closed"));
        }
        if let Some(halves) = halves.as_ref() {
            return Ok(halves.clone());
        }

        let channel = self
            .pending
            .lock()
            .map_err(failure)?
            .take()
            .ok_or_else(|| failure("taken"))?;
        #[cfg(unix)]
        let stream = {
            let stream = channel.into_unix_stream();
            stream.set_nonblocking(true).map_err(failure)?;
            Stream::from_std(stream).map_err(failure)?
        };
        #[cfg(windows)]
        let stream = {
            use std::os::windows::io::IntoRawHandle;
            // SAFETY: subc-os owns a PID-checked Windows duplex pipe supporting asynchronous I/O.
            // Ownership transfers once to Tokio, on the N-API runtime.
            unsafe {
                Stream::from_raw_handle(channel.into_file().into_raw_handle()).map_err(failure)?
            }
        };
        let (read, write) = tokio::io::split(stream);
        let stream = Arc::new((AsyncMutex::new(read), AsyncMutex::new(write)));
        *halves = Some(stream.clone());
        Ok(stream)
    }
}
#[napi]
impl NativeChannel {
    #[napi]
    pub async fn read(&self) -> Result<Buffer> {
        let stop = self.stop.notified();
        tokio::pin!(stop);
        stop.as_mut().enable();
        if self.closed.load(Ordering::Acquire) {
            return Ok(Vec::new().into());
        }
        let stream = self.stream()?;
        let mut reader = stream.0.lock().await;
        let mut bytes = vec![0; 65536];
        let n = tokio::select! { biased; _ = stop => 0, result = reader.read(&mut bytes) => result.map_err(failure)? };
        bytes.truncate(n);
        Ok(bytes.into())
    }
    #[napi]
    pub async fn write(&self, bytes: Buffer) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(failure("closed"));
        }
        let stop = self.stop.notified();
        tokio::pin!(stop);
        stop.as_mut().enable();
        let stream = self.stream()?;
        let mut writer = stream.1.lock().await;
        tokio::select! { biased; _ = stop => Err(failure("closed")), result = writer.write_all(&bytes) => result.map_err(failure) }
    }
    #[napi]
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.stop.notify_waiters();
        if let Ok(mut halves) = self.halves.lock() {
            halves.take();
        }
        if let Ok(mut pending) = self.pending.lock() {
            pending.take();
        }
    }
}

#[napi]
pub struct NativeHandoff {
    inner: Mutex<Option<registration::PreparedHelperClaimHandoff>>,
}
#[napi]
pub fn prepare_helper_claim(child_slot: i32) -> Result<NativeHandoff> {
    Ok(NativeHandoff {
        inner: Mutex::new(Some(
            registration::PreparedHelperClaimHandoff::new(child_slot).map_err(failure)?,
        )),
    })
}
#[napi]
impl NativeHandoff {
    #[napi(getter)]
    pub fn environment(&self) -> Result<std::collections::HashMap<String, String>> {
        Ok(self
            .inner
            .lock()
            .map_err(failure)?
            .as_ref()
            .ok_or_else(|| failure("completed"))?
            .environment()
            .into_iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect())
    }
    #[napi(getter)]
    pub fn child_fd(&self) -> Result<Option<i32>> {
        Ok(self
            .inner
            .lock()
            .map_err(failure)?
            .as_ref()
            .ok_or_else(|| failure("completed"))?
            .child_fd())
    }
    #[napi]
    pub async fn complete(&self, pid: u32, claim: String) -> Result<()> {
        let claim = zeroize::Zeroizing::new(claim);
        let inner = self
            .inner
            .lock()
            .map_err(failure)?
            .take()
            .ok_or_else(|| failure("completed"))?;
        napi::tokio::task::spawn_blocking(move || inner.complete(pid, &claim))
            .await
            .map_err(failure)?
            .map_err(failure)
    }
    #[napi]
    pub fn abort(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.take();
        }
    }
}
#[napi]
pub async fn helper_claim() -> Result<Option<String>> {
    napi::tokio::task::spawn_blocking(|| {
        registration::helper_claim().map(|c| c.map(|c| c.value().to_owned()))
    })
    .await
    .map_err(failure)?
    .map_err(failure)
}
