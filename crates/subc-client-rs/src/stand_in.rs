//! Shared stand-in connection for SDK unit tests and the opt-in public harness.

use super::*;
use tokio::{io::DuplexStream, task::JoinHandle};

pub(crate) struct Served {
    pub(crate) daemon: DuplexStream,
    pub(crate) handle: ModuleHandle,
    pub(crate) serve: JoinHandle<Result<(), SubcModuleError>>,
}

pub(crate) async fn serve_against_stand_in_with_ops<H: ModuleHandler>(
    make_handler: impl FnOnce(ModuleHandle) -> H,
    subc_ops: &[&str],
) -> Served {
    // Start at the acknowledged connection, bypassing discovery, authentication
    // and HELLO's launch nonce. Frames, dispatch and writer shutdown are real.
    let (module, daemon) = tokio::io::duplex(64 * 1024);
    let (read_half, write_half) = tokio::io::split(module);
    let (tx, rx) = mpsc::channel(EGRESS_BUFFER);
    let ack = ModuleHelloAckBody {
        negotiated_ver: PROTOCOL_VERSION,
        subc_ops: subc_ops.iter().map(|op| (*op).to_owned()).collect(),
        subc_capabilities: Vec::new(),
        storage: None,
        machine_id: None,
    };
    let handle = ModuleHandle::new(
        &ack,
        tx.clone(),
        checked_increment(&NEXT_MODULE_CONNECTION_TOKEN).expect("connection token exhausted"),
        CancellationToken::new(),
        tokio::runtime::Handle::current(),
    );
    let handler = Arc::new(make_handler(handle.clone()));
    handler.on_hello_ack(&ack).await;
    let writer = tokio::spawn(drain_writer(write_half, rx));
    let serve = tokio::spawn(connection_serve_future(
        read_half,
        tx,
        handler,
        handle.clone(),
        writer,
    ));
    Served {
        daemon,
        handle,
        serve,
    }
}
