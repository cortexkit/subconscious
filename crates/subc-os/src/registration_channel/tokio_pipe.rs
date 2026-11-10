use super::RegistrationChannel;
use std::{io, os::windows::io::IntoRawHandle};
use tokio::net::windows::named_pipe::NamedPipeClient;

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
