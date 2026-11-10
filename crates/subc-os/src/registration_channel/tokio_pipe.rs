use super::{RegistrationChannel, RegistrationPipe};
use std::{io, os::windows::io::IntoRawHandle};
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer};

impl RegistrationPipe {
    /// Accept the direct child and transfer the owned server to Tokio.
    /// Call on a blocking worker inside a Tokio runtime with I/O enabled.
    pub fn accept_tokio_pipe_server(
        self,
        pid: u32,
        deadline: std::time::Instant,
    ) -> io::Result<NamedPipeServer> {
        let file = self.accept(pid, deadline)?;
        // SAFETY: accept returns a valid, uniquely owned overlapped server
        // handle after checking the child PID. into_raw_handle consumes the
        // File without duplicating the handle; only Tokio owns it afterward.
        #[allow(unsafe_code)]
        unsafe {
            NamedPipeServer::from_raw_handle(file.into_raw_handle())
        }
    }
}

impl RegistrationChannel {
    /// Transfer the validated duplex pipe into Tokio's overlapped transport.
    /// Must be called within a Tokio runtime with I/O enabled.
    pub fn into_tokio_pipe_client(self) -> io::Result<NamedPipeClient> {
        // SAFETY: this channel uniquely owns an overlapped pipe handle, opened
        // and verified by subc-os. into_raw_handle consumes the File, and neither
        // it nor the channel uses the handle again after Tokio takes ownership.
        #[allow(unsafe_code)]
        unsafe {
            NamedPipeClient::from_raw_handle(self.into_file().into_raw_handle())
        }
    }
}
