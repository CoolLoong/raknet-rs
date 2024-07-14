use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};

use flume::{Receiver, Sender};
use futures::{SinkExt, Stream};
use log::{debug, error, info};
use minitrace::collector::SpanContext;
use minitrace::Span;
use pin_project_lite::pin_project;
use tokio::net::UdpSocket as TokioUdpSocket;
use tokio_util::udp::UdpFramed;

use super::{Config, MakeIncoming};
use crate::codec::tokio::Codec;
use crate::codec::{Decoded, Encoded};
use crate::errors::CodecError;
use crate::guard::HandleOutgoing;
use crate::io::{IOImpl, IO};
use crate::link::TransferLink;
use crate::packet::connected::{self, Frames, FramesMut};
use crate::packet::{unconnected, Packet};
use crate::server::handler::offline;
use crate::server::handler::offline::HandleOffline;
use crate::server::handler::online::HandleOnline;
use crate::utils::{Log, Logged, StreamExt};

type OfflineHandler = offline::OfflineHandler<
    Log<UdpFramed<Codec, Arc<TokioUdpSocket>>, (Packet<FramesMut>, SocketAddr), CodecError>,
>;

pin_project! {
    struct Incoming {
        #[pin]
        offline: OfflineHandler,
        config: Config,
        socket: Arc<TokioUdpSocket>,
        router: HashMap<SocketAddr, Sender<connected::Packet<FramesMut>>>,
        drop_receiver: Receiver<SocketAddr>,
        drop_notifier: Sender<SocketAddr>,
    }
}

impl Incoming {
    fn clear_dropped_addr(self: Pin<&mut Self>) {
        let mut this = self.project();
        for addr in this.drop_receiver.try_iter() {
            this.router.remove(&addr);
            this.offline.as_mut().disconnect(&addr);
        }
    }
}

impl MakeIncoming for TokioUdpSocket {
    fn make_incoming(self, config: Config) -> impl Stream<Item = impl IO> {
        let socket = Arc::new(self);
        let (drop_notifier, drop_receiver) = flume::unbounded();
        Incoming {
            offline: UdpFramed::new(Arc::clone(&socket), Codec)
                .logged_err(|err| {
                    debug!("[server] got codec error: {err} when decode offline frames");
                })
                .handle_offline(config.offline_config()),
            socket,
            config,
            router: HashMap::new(),
            drop_receiver,
            drop_notifier,
        }
    }
}

impl Stream for Incoming {
    type Item = impl IO;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.as_mut().clear_dropped_addr();

        let mut this = self.project();
        loop {
            let Some((pack, peer)) = ready!(this.offline.as_mut().poll_next(cx)) else {
                return Poll::Ready(None);
            };
            if let Some(router_tx) = this.router.get_mut(&peer.addr) {
                if router_tx.send(pack).is_err() {
                    error!("[server] connection was dropped before closed");
                    this.router.remove(&peer.addr);
                    this.offline.as_mut().disconnect(&peer.addr);
                }
                continue;
            }
            info!("[server] new incoming from {}", peer.addr);

            let (router_tx, router_rx) = flume::unbounded();
            router_tx.send(pack).unwrap();
            this.router.insert(peer.addr, router_tx);

            let ack = TransferLink::new_arc(this.config.server_role());

            let write = UdpFramed::new(Arc::clone(this.socket), Codec)
                .handle_outgoing(
                    Arc::clone(&ack),
                    this.config.send_buf_cap,
                    peer.clone(),
                    this.config.server_role(),
                )
                .frame_encoded(peer.mtu, this.config.codec_config(), Arc::clone(&ack));

            let raw_write = UdpFramed::new(Arc::clone(this.socket), Codec).with(
                move |input: unconnected::Packet| async move {
                    Ok((Packet::<Frames>::Unconnected(input), peer.addr))
                },
            );

            let io = ack
                .filter_incoming_ack(router_rx.into_stream())
                .frame_decoded(
                    this.config.codec_config(),
                    Arc::clone(&ack),
                    this.config.server_role(),
                )
                .handle_online(
                    write,
                    raw_write,
                    peer.addr,
                    this.config.sever_guid,
                    this.drop_notifier.clone(),
                )
                .enter_on_item(move || {
                    Span::root("conn", SpanContext::random()).with_properties(|| {
                        [
                            ("addr", peer.addr.to_string()),
                            ("mtu", peer.mtu.to_string()),
                        ]
                    })
                });

            return Poll::Ready(Some(IOImpl::new(io)));
        }
    }
}
