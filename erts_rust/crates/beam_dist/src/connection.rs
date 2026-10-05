//! The connection-level API: the handshake driver and a live distribution link.
//!
//! Drives the connect/challenge handshake (client and server roles) and then
//! sends/receives distributed `DOP_*` control messages over a
//! [`crate::transport::Transport`], including a `TcpStream` adapter.
//! Implementation pending.
