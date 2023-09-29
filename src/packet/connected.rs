use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::errors::CodecError;
use crate::packet::PackId;

// Packet when RakNet has established a connection
#[derive(Debug)]
pub(crate) enum Packet<T: Buf = Bytes> {
    FrameSet(FrameSet<T>),
    Ack(Ack),
    Nack(Ack),
}

#[derive(Debug)]
pub(crate) struct FrameSet<T: Buf = Bytes> {
    seq_num: Uint24le,
    flags: Flags,
    reliable_frame_index: Option<Uint24le>,
    seq_frame_index: Option<Uint24le>,
    ordered_frame_index: Option<Uint24le>,
    // ignored
    // ordered_channel: u8,
    fragment: Option<Fragment>,
    body: T,
}

impl FrameSet {
    /// Get the inner packet id
    pub(crate) fn inner_pack_id(&self) -> Result<PackId, CodecError> {
        PackId::from_u8(
            *self
                .body
                .chunk()
                .first()
                .ok_or(CodecError::InvalidPacketLength)?,
        )
    }

    fn read(buf: &mut BytesMut) -> Result<Self, CodecError> {
        let seq_num = Uint24le::read(buf);
        let flags = Flags::read(buf);
        // length in bytes
        let length = buf.get_u16() >> 3;
        if length == 0 {
            return Err(CodecError::InvalidPacketLength);
        }
        let reliability = flags.reliability()?;
        let mut reliable_frame_index = None;
        let mut seq_frame_index = None;
        let mut ordered_frame_index = None;
        let mut fragment = None;

        if reliability.is_reliable() {
            reliable_frame_index = Some(Uint24le::read(buf));
        }
        if reliability.is_sequenced() {
            seq_frame_index = Some(Uint24le::read(buf));
        }
        if reliability.is_sequenced_or_ordered() {
            ordered_frame_index = Some(Uint24le::read(buf));
            // skip the order channel (u8)
            buf.advance(1);
        }
        if flags.parted() {
            fragment = Some(Fragment::read(buf));
        }
        Ok(FrameSet {
            seq_num,
            flags,
            reliable_frame_index,
            seq_frame_index,
            ordered_frame_index,
            fragment,
            body: buf.split_to(length as usize).freeze(),
        })
    }

    fn write(self, buf: &mut BytesMut) {
        self.seq_num.write(buf);
        self.flags.write(buf);
        // length in bits
        // self.body will be split up so cast to u16 should not overflow here
        debug_assert!(
            self.body.len() < (u16::MAX >> 3) as usize,
            "self.body should be constructed based on mtu"
        );
        buf.put_u16((self.body.len() << 3) as u16);
        if let Some(reliable_frame_index) = self.reliable_frame_index {
            reliable_frame_index.write(buf);
        }
        if let Some(seq_frame_index) = self.seq_frame_index {
            seq_frame_index.write(buf);
        }
        if let Some(ordered_frame_index) = self.ordered_frame_index {
            ordered_frame_index.write(buf);
            // skip the order channel (u8)
            buf.put_u8(0);
        }
        if let Some(fragment) = self.fragment {
            fragment.write(buf);
        }
        buf.put(self.body);
    }
}

/// `uint24` little-endian but actually occupies 4 bytes.
#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
pub(crate) struct Uint24le(u32);

impl Uint24le {
    fn read(buf: &mut BytesMut) -> Self {
        // safe cast because only 3 bytes will not overflow
        Self(buf.get_uint_le(3) as u32)
    }

    fn write(self, buf: &mut BytesMut) {
        buf.put_uint_le(self.0 as u64, 3);
    }
}

/// Top 3 bits are reliability type, fourth bit is 1 when the frame is fragmented and part of a
/// compound.
#[derive(Debug)]
struct Flags(u8);

#[derive(Debug)]
#[repr(u8)]
enum Reliability {
    /// Unreliable packets are sent by straight UDP. They may arrive out of order, or not at all.
    /// This is best for data that is unimportant, or data that you send very frequently so even if
    /// some packets are missed newer packets will compensate. Advantages - These packets don't
    /// need to be acknowledged by the network, saving the size of a UDP header in acknowledgment
    /// (about 50 bytes or so). The savings can really add up. Disadvantages - No packet
    /// ordering, packets may never arrive, these packets are the first to get dropped if the send
    /// buffer is full.
    Unreliable = 0x00,
    /// Unreliable sequenced packets are the same as unreliable packets, except that only the
    /// newest packet is ever accepted. Older packets are ignored. Advantages - Same low overhead
    /// as unreliable packets, and you don't have to worry about older packets changing your data
    /// to old values. Disadvantages - A LOT of packets will be dropped since they may never
    /// arrive because of UDP and may be dropped even when they do arrive. These packets are the
    /// first to get dropped if the send buffer is full. The last packet sent may never arrive,
    /// which can be a problem if you stop sending packets at some particular point.
    UnreliableSequenced = 0x01,
    /// Reliable packets are UDP packets monitored by a reliability layer to ensure they arrive at
    /// the destination. Advantages - You know the packet will get there. Eventually...
    /// Disadvantages - Retransmissions and acknowledgments can add significant bandwidth
    /// requirements. Packets may arrive very late if the network is busy. No packet ordering.
    Reliable = 0x02,
    /// Reliable ordered packets are UDP packets monitored by a reliability layer to ensure they
    /// arrive at the destination and are ordered at the destination. Advantages - The packet will
    /// get there and in the order it was sent. These are by far the easiest to program for because
    /// you don't have to worry about strange behavior due to out of order or lost packets.
    /// Disadvantages - Retransmissions and acknowledgments can add significant bandwidth
    /// requirements. Packets may arrive very late if the network is busy. One late packet can
    /// delay many packets that arrived sooner, resulting in significant lag spikes. However, this
    /// disadvantage can be mitigated by the clever use of ordering streams .
    ReliableOrdered = 0x03,
    /// Reliable sequenced packets are UDP packets monitored by a reliability layer to ensure they
    /// arrive at the destination and are sequenced at the destination. Advantages - You get
    /// the reliability of UDP packets, the ordering of ordered packets, yet don't have to wait for
    /// old packets. More packets will arrive with this method than with the unreliable sequenced
    /// method, and they will be distributed more evenly. The most important advantage however is
    /// that the latest packet sent will arrive, where with unreliable sequenced the latest packet
    /// sent may not arrive. Disadvantages - Wasteful of bandwidth because it uses the overhead
    /// of reliable UDP packets to ensure late packets arrive that just get ignored anyway.
    ReliableSequenced = 0x04,
}

impl Reliability {
    fn is_reliable(&self) -> bool {
        matches!(
            self,
            Reliability::Reliable | Reliability::ReliableSequenced | Reliability::ReliableOrdered
        )
    }

    fn is_sequenced_or_ordered(&self) -> bool {
        matches!(
            self,
            Reliability::ReliableSequenced
                | Reliability::ReliableOrdered
                | Reliability::UnreliableSequenced
        )
    }

    fn is_sequenced(&self) -> bool {
        matches!(
            self,
            Reliability::UnreliableSequenced | Reliability::ReliableSequenced
        )
    }
}

impl Flags {
    fn read(buf: &mut BytesMut) -> Self {
        Self(buf.get_u8())
    }

    fn write(self, buf: &mut BytesMut) {
        buf.put_u8(self.0);
    }

    /// Get the reliability of this flags
    fn reliability(&self) -> Result<Reliability, CodecError> {
        let r = self.0 >> 5;
        if r > Reliability::ReliableSequenced as u8 {
            return Err(CodecError::InvalidReliability(r));
        }
        // Safety:
        // It is checked before transmute
        unsafe { Ok(std::mem::transmute(r)) }
    }

    /// Return if it is parted
    fn parted(&self) -> bool {
        // 0b0001_0000
        const PARTED_FLAG: u8 = 0x10;

        self.0 & PARTED_FLAG != 0
    }
}

#[derive(Debug)]
struct Fragment {
    parted_size: u32,
    parted_id: u16,
    parted_index: u32,
}

impl Fragment {
    fn read(buf: &mut BytesMut) -> Self {
        Self {
            parted_size: buf.get_u32(),
            parted_id: buf.get_u16(),
            parted_index: buf.get_u32(),
        }
    }

    fn write(self, buf: &mut BytesMut) {
        buf.put_u32(self.parted_size);
        buf.put_u16(self.parted_id);
        buf.put_u32(self.parted_index);
    }
}

#[derive(Debug)]
pub(crate) struct Ack {
    records: Vec<Record>,
}

impl Ack {
    /// Extend an ack packet from a sorted sequence numbers iterator based on mtu.
    /// Notice that a uint24le must be unique in the whole iterator
    pub(crate) fn extend_from<I: Iterator<Item = Uint24le>>(
        mut sorted_seq_nums: I,
        mut mtu: u16,
    ) -> Option<Self> {
        // pack_id(1) + length(2) + single record(4) = 7
        debug_assert!(mtu >= 7, "7 is the least size of mtu");
        let mut records = Vec::new();
        let Some(mut first) = sorted_seq_nums.next() else {
            return None;
        };
        let mut last = first;
        let mut upgrade_flag = true;
        // first byte is pack_id, next 2 bytes are length, the first seq_num takes at least 4 bytes
        mtu -= 7;
        loop {
            // we cannot poll sorted_seq_nums because 4 is the least size of a record
            if mtu < 4 {
                break;
            }
            let Some(seq_num) = sorted_seq_nums.next() else {
                break;
            };
            if seq_num.0 == last.0 + 1 {
                if upgrade_flag {
                    mtu -= 3;
                    upgrade_flag = false;
                }
                last = seq_num;
                continue;
            }
            mtu -= 4;
            upgrade_flag = true;
            if first.0 != last.0 {
                records.push(Record::Range(first, last));
            } else {
                records.push(Record::Single(first));
            }
            first = seq_num;
            last = seq_num;
        }

        if first.0 != last.0 {
            records.push(Record::Range(first, last));
        } else {
            records.push(Record::Single(first));
        }

        Some(Self { records })
    }

    fn read(buf: &mut BytesMut) -> Result<Self, CodecError> {
        const MAX_ACKNOWLEDGEMENT_PACKETS: u32 = 8192;

        let mut ack_cnt = 0;
        let record_cnt = buf.get_u16();
        let mut records = Vec::with_capacity(record_cnt as usize);
        for _ in 0..record_cnt {
            let record = Record::read(buf)?;
            ack_cnt += record.ack_cnt();
            if ack_cnt > MAX_ACKNOWLEDGEMENT_PACKETS {
                return Err(CodecError::AckCountExceed);
            }
            records.push(record);
        }
        Ok(Self { records })
    }

    fn write(self, buf: &mut BytesMut) {
        debug_assert!(
            self.records.len() < u16::MAX as usize,
            "self.records should be constructed based on mtu"
        );
        buf.put_u16(self.records.len() as u16);
        for record in self.records {
            record.write(buf);
        }
    }
}

const RECORD_RANGE: u8 = 0;
const RECORD_SINGLE: u8 = 1;

#[derive(Debug)]
pub(crate) enum Record {
    Range(Uint24le, Uint24le),
    Single(Uint24le),
}

impl Record {
    fn read(buf: &mut BytesMut) -> Result<Self, CodecError> {
        let record_type = buf.get_u8();
        match record_type {
            RECORD_RANGE => Ok(Record::Range(Uint24le::read(buf), Uint24le::read(buf))),
            RECORD_SINGLE => Ok(Record::Single(Uint24le::read(buf))),
            _ => Err(CodecError::InvalidRecordType(record_type)),
        }
    }

    fn write(self, buf: &mut BytesMut) {
        match self {
            Record::Range(start, end) => {
                buf.put_u8(RECORD_RANGE);
                start.write(buf);
                end.write(buf);
            }
            Record::Single(idx) => {
                buf.put_u8(RECORD_SINGLE);
                idx.write(buf);
            }
        }
    }

    fn ack_cnt(&self) -> u32 {
        match self {
            Record::Range(start, end) => end.0 - start.0 + 1,
            Record::Single(_) => 1,
        }
    }
}

impl Packet {
    pub(super) fn pack_id(&self) -> PackId {
        match self {
            Packet::FrameSet(_) => PackId::FrameSet,
            Packet::Ack(_) => PackId::Ack,
            Packet::Nack(_) => PackId::Nack,
        }
    }

    pub(super) fn read_frame_set(buf: &mut BytesMut) -> Result<Self, CodecError> {
        Ok(Packet::FrameSet(FrameSet::read(buf)?))
    }

    pub(super) fn read_ack(buf: &mut BytesMut) -> Result<Self, CodecError> {
        Ok(Packet::Ack(Ack::read(buf)?))
    }

    pub(super) fn read_nack(buf: &mut BytesMut) -> Result<Self, CodecError> {
        Ok(Packet::Nack(Ack::read(buf)?))
    }

    pub(super) fn write(self, buf: &mut BytesMut) {
        match self {
            Packet::FrameSet(frame) => frame.write(buf),
            Packet::Ack(ack) | Packet::Nack(ack) => ack.write(buf),
        }
    }
}

// enum BodyPacket {
//     ConnectedPing {
//         client_timestamp: i64,
//     },
//     ConnectedPong {
//         client_timestamp: i64,
//         server_timestamp: i64,
//     },
//     ConnectionRequest {
//         client_guid: u64,
//         request_timestamp: i64,
//         use_encryption: bool,
//     },
//     ConnectionRequestAccepted {
//         client_address: std::net::SocketAddr,
//         // system_index: u16,
//         system_addresses: [std::net::SocketAddr; 10],
//         request_timestamp: i64,
//         accepted_timestamp: i64,
//     },
//     AlreadyConnected {
//         magic: bool,
//         server_guid: u64,
//     },
//     NewIncomingConnection {
//         server_address: std::net::SocketAddr,
//         system_addresses: [std::net::SocketAddr; 10],
//         request_timestamp: i64,
//         accepted_timestamp: i64,
//     },
//     Disconnect,
//     IncompatibleProtocolVersion {
//         server_protocol: u8,
//         magic: bool,
//         server_guid: u64,
//     },
//     Game,
// }

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_ack_should_not_overflow_mtu() {
        let mtu: u16 = 21;
        let mut buf = BytesMut::with_capacity(mtu as usize);

        let test_cases = [
            // 3 + 0-2(7) + 4-5(7) + 7(4) = 21, remain 8
            (vec![0, 1, 2, 4, 5, 7, 8], 21, 1),
            // 3 + 0-1(7) + 3-4(7) + 6(4) = 21, remain 7, 9
            (vec![0, 1, 3, 4, 6, 7, 9], 21, 2),
            // 3 + 0(4) + 2(4) + 4(4) + 6(4) = 19, remain 8, 10, 12
            (vec![0, 2, 4, 6, 8, 10, 12], 19, 3),
            // 3 + 0(4) + 2(4) + 5-6(7) = 18, remain 8, 9, 12
            (vec![0, 2, 5, 6, 8, 9, 12], 18, 3),
            // 3 + 0-1(7) = 10, no remain
            (vec![0, 1], 10, 0),
            // 3 + 0(4) + 2-3(7) = 14, no remain
            (vec![0, 2, 3], 14, 0),
            // 3 + 0(4) + 2(4) + 4(4) = 15, no remain
            (vec![0, 2, 4], 15, 0),
        ];
        for (seq_nums, len, remain) in test_cases {
            buf.clear();
            // pack id
            buf.put_u8(0);
            let mut seq_nums = seq_nums.into_iter().map(Uint24le);
            let ack = Ack::extend_from(&mut seq_nums, mtu).unwrap();
            ack.write(&mut buf);
            assert_eq!(buf.len(), len);
            assert_eq!(seq_nums.len(), remain);
        }
    }
}
