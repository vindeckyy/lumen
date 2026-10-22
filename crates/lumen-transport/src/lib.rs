//! Lumen transport layer.
//!
//! Media travels over UDP datagrams: video with FEC and deadline-aware
//! selective retransmission, audio and input on their own sockets. The
//! control channel is TCP and lives in the host/client crates, because it
//! carries structured control messages rather than framed media.

#![forbid(unsafe_code)]

pub mod estimator;
pub mod fec;
pub mod inject;
pub mod input_channel;
pub mod mdns;
pub mod media;
pub mod nack;
pub mod pacing;
pub mod retransmit;
pub mod slice;
pub mod socket;
pub mod wol;

pub use estimator::Estimator;
pub use fec::{BLOCK_DATA_SLICES, BLOCK_PARITY_SLICES, FecBlock, ParityLevel};
pub use inject::{InjectConfig, Injector};
pub use media::{MediaError, MediaReceiver, MediaSender, ReceiverStats, send_paced};
pub use pacing::Pacer;
pub use retransmit::{MissingSlice, RetransmitQueue};
pub use slice::{ReceivedSlice, SliceAssembler};
pub use socket::{MediaSocket, SocketOptions};
pub use wol::{MacAddr, broadcast_targets, magic_packet};
