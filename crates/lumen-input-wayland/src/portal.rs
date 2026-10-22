//! InputCapture portal: opens a capture session and returns its EIS socket.
//!
//! The InputCapture portal is the receiver side of EIS. Captured local input arrives on the
//! returned socket as libei events. The RemoteDesktop portal's `ConnectToEIS` is the sender side
//! and carries no local input, so it is not used here.

use std::os::fd::OwnedFd;

use ashpd::desktop::Session;
use ashpd::desktop::input_capture::{
    Capabilities, ConnectToEISOptions, CreateSessionOptions, EnableOptions, InputCapture,
};
use tokio::runtime::Runtime;

/// An open capture session and the runtime that drives its D-Bus traffic. Both must outlive the
/// EIS socket, because the portal ends capture when the session closes.
pub(crate) struct PortalSession {
    _session: Session<InputCapture>,
    _runtime: Runtime,
}

/// Opens a keyboard and pointer capture session. Returns the session holder and the EIS socket.
pub(crate) fn connect() -> Result<(PortalSession, OwnedFd), String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime: {e}"))?;
    let (session, fd) = runtime.block_on(async {
        let portal = InputCapture::new().await.map_err(|e| e.to_string())?;
        let (session, _capabilities) = portal
            .create_session(
                None,
                CreateSessionOptions::default()
                    .set_capabilities(Capabilities::Keyboard | Capabilities::Pointer),
            )
            .await
            .map_err(|e| e.to_string())?;
        portal
            .enable(&session, EnableOptions::default())
            .await
            .map_err(|e| e.to_string())?;
        let fd = portal
            .connect_to_eis(&session, ConnectToEISOptions::default())
            .await
            .map_err(|e| e.to_string())?;
        Ok::<_, String>((session, fd))
    })?;
    Ok((
        PortalSession {
            _session: session,
            _runtime: runtime,
        },
        fd,
    ))
}
