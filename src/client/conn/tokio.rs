use std::io;
use std::net::ToSocketAddrs;
use std::sync::Arc;

use fastrace::Span;
use futures::StreamExt;
use tokio::net::UdpSocket as TokioUdpSocket;

use super::ConnectTo;
use crate::client::handler::offline::OfflineHandler;
use crate::client::handler::online::HandleOnline;
use crate::codec::frame::Framed;
use crate::codec::{Decoded, Encoded};
use crate::guard::HandleOutgoing;
use crate::io::{Ping, SeparatedIO, IO};
use crate::link::{Router, TransferLink};
use crate::state::{IncomingStateManage, OutgoingStateManage};
use crate::utils::TraceStreamExt;

impl ConnectTo for TokioUdpSocket {
    async fn connect_to(
        self,
        addrs: impl ToSocketAddrs,
        config: super::Config,
    ) -> io::Result<impl IO + Ping> {
        let socket = Arc::new(self);
        let mut lookups = addrs.to_socket_addrs()?;
        let addr = loop {
            if let Some(addr) = lookups.next() {
                if socket.connect(addr).await.is_ok() {
                    break addr;
                }
                continue;
            }
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "invalid address",
            ));
        };

        let (mut incoming, peer) = OfflineHandler::new(
            Framed::new(Arc::clone(&socket), config.mtu as usize), // TODO: discover MTU
            addr,
            config.offline_config(),
        )
        .await?;
        let role = config.client_role();

        let link = TransferLink::new_arc(role, peer);
        let dst = Framed::new(Arc::clone(&socket), peer.mtu as usize)
            .handle_outgoing(Arc::clone(&link), config.send_buf_cap, peer, role)
            .frame_encoded(peer.mtu, config.codec_config(), Arc::clone(&link))
            .manage_outgoing_state(None);

        let (mut router, route) = Router::new(Arc::clone(&link));

        tokio::spawn(async move {
            while let Some(pack) = incoming.next().await {
                router.deliver(pack);
            }
        });

        let src = route
            .frame_decoded(config.codec_config(), role, peer)
            .manage_incoming_state()
            .handle_online(addr, config.client_guid, Arc::clone(&link))
            .enter_on_item(Span::noop);

        Ok(SeparatedIO::new(src, dst))
    }
}
