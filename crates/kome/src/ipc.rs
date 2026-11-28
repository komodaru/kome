//! プロセス間の通信路: 上限つきのメッセージの列と、バイトの流れ。
//!
//! kome が持つのは、容量の決まった入れ物、両端の保持、読める・書けるの通知である。名前つき
//! パイプ、ソケット、ポートといった個々の OS の意味は Personality がこの上に定義する。
//!
//! - **形**: [`ChannelSpec::messages`] は境界を保つ (一度の送信が一度の受信になる)。
//!   [`ChannelSpec::stream`] は境界の無いバイトの流れで、入るぶんだけ受け付ける。
//! - **端**: 送る端と受ける端 ([`End`]) を、プロセスが別々に保持する。送る端の保持が無くなると、
//!   受け手は残りを読み切った後に [`Received::Ended`] を受け取る。受ける端の保持が無くなると、
//!   残っていた中身は捨てられ、送信は [`Sent::NoReceiver`] になる。両方が無くなれば回収する。
//! - **通知**: 通信路は、読めるときに数が 1 になる資源と、書けるときに数が 1 になる資源を持つ。
//!   スレッドは [`Kernel::wait`] でこれを待つ。数は kome が中身から決め、外からは変えられない。
//! - **資源の受け渡し**: メッセージは、待機可能な資源か実体の保持を一つ運べる ([`Parcel`])。
//!   送り手の保持は送信で通信路へ移り、受信で受け手へ移る。運ばれている間、資源は破棄されない。
//!
//! 容量は作るときに確保する。送信と解放は確保しない。

use crate::error::{Error, Invariant, Refusal, Resource};
use crate::id::{BackingId, ChannelId, ObjectId, ProcessId};
use crate::kernel::Kernel;
use crate::object::Object;
use crate::store::{Queue, TryClone, plain_try_clone};

/// 通信路の端。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// 送る端。
    Send,
    /// 受ける端。
    Receive,
}

/// 通信路の形。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// 境界を保つメッセージの列。
    Messages {
        /// 持てるメッセージの数。
        count: u32,
        /// 一つのメッセージの長さの上限。
        max_len: u32,
    },
    /// 境界の無いバイトの流れ。
    Stream {
        /// 持てるバイト数。
        bytes: u32,
    },
}

/// 通信路の形と容量。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelSpec {
    framing: Framing,
}

impl ChannelSpec {
    /// 境界を保つメッセージの列。`count` 個まで持ち、一つは `max_len` バイトまで。
    #[must_use]
    pub const fn messages(count: u32, max_len: u32) -> ChannelSpec {
        ChannelSpec { framing: Framing::Messages { count, max_len } }
    }

    /// 境界の無いバイトの流れ。`bytes` バイトまで持つ。
    #[must_use]
    pub const fn stream(bytes: u32) -> ChannelSpec {
        ChannelSpec { framing: Framing::Stream { bytes } }
    }

    /// 形と容量。
    #[must_use]
    pub const fn framing(&self) -> Framing {
        self.framing
    }

    /// `len` バイトの送信のうち、受け付けるバイト数。空きが無ければ `None`。
    ///
    /// `queued` は持っているバイト数、`frames` は持っているメッセージの数。整数だけで決まる。
    /// 受け付けた送信が通信路に収まることは、Kani の `an_admitted_send_fits_the_channel` が示す。
    pub(crate) fn admit(self, len: u32, queued: u64, frames: u64, parcel: bool) -> Result<Option<u32>, Refusal> {
        match self.framing {
            Framing::Messages { max_len, .. } if len > max_len => Err(Refusal::MessageTooLong),
            Framing::Messages { count, .. } => Ok((frames < u64::from(count)).then_some(len)),
            Framing::Stream { .. } if parcel => Err(Refusal::ParcelOnStream),
            Framing::Stream { bytes } => {
                let free = u64::from(bytes).saturating_sub(queued);
                Ok((free != 0).then(|| len.min(u32::try_from(free).unwrap_or(u32::MAX))))
            }
        }
    }

    /// 持てるメッセージの数と、持てるバイト数。
    pub(crate) fn sizes(self) -> Option<(u32, u64)> {
        match self.framing {
            Framing::Messages { count, max_len } if count != 0 => {
                Some((count, u64::from(count).checked_mul(u64::from(max_len))?))
            }
            Framing::Stream { bytes } if bytes != 0 => Some((0, u64::from(bytes))),
            Framing::Messages { .. } | Framing::Stream { .. } => None,
        }
    }
}

/// メッセージが運ぶ資源の保持。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parcel {
    /// 待機可能な資源の保持。
    Object(ObjectId),
    /// 実体の保持 (共有メモリの受け渡し)。
    Backing(BackingId),
}

/// 通信路を作ったときに返すもの。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelCreated {
    channel: ChannelId,
    readable: ObjectId,
    writable: ObjectId,
}

impl ChannelCreated {
    /// 通信路。
    #[must_use]
    pub const fn channel(&self) -> ChannelId {
        self.channel
    }

    /// 読めるとき (中身がある、または送る端が無い) に数が 1 になる資源。
    #[must_use]
    pub const fn readable(&self) -> ObjectId {
        self.readable
    }

    /// 書けるとき (空きがある、または受ける端が無い) に数が 1 になる資源。
    #[must_use]
    pub const fn writable(&self) -> ObjectId {
        self.writable
    }
}

/// 送信の結果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    /// このバイト数を受け付けた。メッセージは全部、流れは入ったぶん。
    Accepted(u64),
    /// 空きが無い。何も変えていない。
    Full,
    /// 受ける端が無い。何も変えていない。
    NoReceiver,
}

/// 受信の結果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Received {
    /// バッファの先頭へこのバイト数を書いた。メッセージが資源を運んでいれば、その保持は受け手へ移った。
    Data {
        /// 書いたバイト数。
        bytes: u64,
        /// 受け取った資源の保持。
        parcel: Option<Parcel>,
    },
    /// 中身が無い。送る端はまだある。
    Empty,
    /// 中身が無く、送る端も無い。これ以上は届かない。
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) len: u32,
    pub(crate) parcel: Option<Parcel>,
}

plain_try_clone!(Frame, u8);

impl TryClone for Channel {
    fn try_clone(&self) -> Result<Channel, Error> {
        let Channel { spec, capacity, bytes, frames, senders, receivers, readable, writable } = self;
        Ok(Channel {
            spec: *spec,
            capacity: *capacity,
            bytes: bytes.try_clone()?,
            frames: frames.try_clone()?,
            senders: *senders,
            receivers: *receivers,
            readable: *readable,
            writable: *writable,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Channel {
    spec: ChannelSpec,
    /// 持てるバイト数。
    pub(crate) capacity: u64,
    pub(crate) bytes: Queue<u8>,
    /// メッセージの列でだけ使う。メッセージごとの長さと、運んでいる資源。
    pub(crate) frames: Queue<Frame>,
    /// 送る端の保持の合計。
    pub(crate) senders: u32,
    /// 受ける端の保持の合計。
    pub(crate) receivers: u32,
    pub(crate) readable: ObjectId,
    pub(crate) writable: ObjectId,
}

impl Channel {
    fn queued(&self) -> u64 {
        u64::try_from(self.bytes.len()).unwrap_or(u64::MAX)
    }

    /// 持てるメッセージの数 (流れでは 0)。
    pub(crate) fn frame_limit(&self) -> usize {
        match self.spec.framing {
            Framing::Messages { count, .. } => usize::try_from(count).unwrap_or(usize::MAX),
            Framing::Stream { .. } => 0,
        }
    }

    pub(crate) fn has_content(&self) -> bool {
        match self.spec.framing {
            Framing::Messages { .. } => !self.frames.is_empty(),
            Framing::Stream { .. } => !self.bytes.is_empty(),
        }
    }

    pub(crate) fn has_room(&self) -> bool {
        match self.spec.framing {
            Framing::Messages { .. } => self.frames.len() < self.frame_limit(),
            Framing::Stream { .. } => self.queued() < self.capacity,
        }
    }

    /// 読めるか (中身がある、または送る端が無い)。
    pub(crate) fn readable_now(&self) -> bool {
        self.has_content() || self.senders == 0
    }

    /// 書けるか (空きがある、または受ける端が無い)。
    pub(crate) fn writable_now(&self) -> bool {
        self.has_room() || self.receivers == 0
    }

    /// メッセージの長さの合計が、持っているバイト数と一致するか (流れでは列が空であること)。
    pub(crate) fn framed(&self) -> bool {
        let total = self.frames.iter().map(|frame| u64::from(frame.len)).fold(0u64, u64::saturating_add);
        match self.spec.framing {
            Framing::Messages { max_len, .. } => {
                total == self.queued() && self.frames.iter().all(|frame| frame.len <= max_len)
            }
            Framing::Stream { .. } => self.frames.is_empty(),
        }
    }

    /// 長さの記録を書き換える。検査が違反を検出できることを確かめる試験のための入口。
    #[cfg(test)]
    pub(crate) fn corrupt_senders(&mut self, senders: u32) {
        self.senders = senders;
    }
}

impl Kernel {
    /// 通信路を作る。`process` が送る端と受ける端を一つずつ保持する。
    ///
    /// 読める・書けるの通知の資源は誰にも保持されていない。通信路がある間は破棄されない。
    /// それより長く使うプロセスは [`Kernel::retain_object`] で保持する。
    pub fn create_channel(&mut self, process: ProcessId, spec: ChannelSpec) -> Result<ChannelCreated, Error> {
        let (count, capacity) = spec.sizes().ok_or(Error::Refused(Refusal::BadChannelSpec))?;
        let total = self
            .channel_bytes
            .checked_add(capacity)
            .filter(|total| *total <= self.limits.channel_bytes())
            .ok_or(Error::Exhausted(Resource::ChannelBytes))?;
        let limit = self.holdings_limit(process);
        let owner = self.alive_mut(process)?;
        owner.senders.room_for_new(limit)?;
        owner.receivers.room_for_new(limit)?;
        self.channels.reserve()?;
        self.objects.reserve_many(2)?;
        let mut bytes = Queue::new();
        bytes.reserve_total(usize::try_from(capacity).map_err(|_| Error::Exhausted(Resource::HostMemory))?)?;
        let mut frames = Queue::new();
        frames.reserve_total(usize::try_from(count).map_err(|_| Error::Exhausted(Resource::HostMemory))?)?;
        // 通知の資源二つの破棄。
        self.effects.promise(2)?;
        let readable = self.objects.insert(Object::driven(false))?;
        let writable = self.objects.insert(Object::driven(true))?;
        let channel = self.channels.insert(Channel {
            spec,
            capacity,
            bytes,
            frames,
            senders: 1,
            receivers: 1,
            readable,
            writable,
        })?;
        let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ChannelKept))?;
        owner.senders.add(channel, limit)?;
        owner.receivers.add(channel, limit)?;
        self.channel_bytes = total;
        Ok(ChannelCreated { channel, readable, writable })
    }

    /// `process` の、`end` の保持を一つ増やす。保持が無くなった端は増やせない。
    pub fn retain_channel(&mut self, process: ProcessId, channel: ChannelId, end: End) -> Result<(), Error> {
        let limit = self.holdings_limit(process);
        let entry = self.channels.find(channel, &self.lineage)?;
        let held = match end {
            End::Send => entry.senders,
            End::Receive => entry.receivers,
        };
        if held == 0 {
            return Err(Error::Refused(Refusal::EndClosed));
        }
        let held = held.checked_add(1).ok_or(Error::Refused(Refusal::HoldOverflow))?;
        let owner = self.alive_mut(process)?;
        match end {
            End::Send => owner.senders.add(channel, limit)?,
            End::Receive => owner.receivers.add(channel, limit)?,
        }
        let entry = self.channels.get_mut(channel).ok_or(Error::Broken(Invariant::ChannelKept))?;
        match end {
            End::Send => entry.senders = held,
            End::Receive => entry.receivers = held,
        }
        Ok(())
    }

    /// `process` の、`end` の保持を一つ減らす。
    pub fn release_channel(&mut self, process: ProcessId, channel: ChannelId, end: End) -> Result<(), Error> {
        self.channels.find(channel, &self.lineage)?;
        let owner = self.processes.find_mut(process, &self.lineage)?;
        match end {
            End::Send => owner.senders.sub(channel)?,
            End::Receive => owner.receivers.sub(channel)?,
        }
        self.drop_end(channel, end)
    }

    /// `bytes` を送る。`process` は送る端を保持していなければならない。
    ///
    /// メッセージの列では、全部を受け付けるか、何も受け付けない。`parcel` があれば、`process` の
    /// その資源の保持が一つ、メッセージへ移る。流れでは、空いているぶんだけ受け付ける。
    pub fn send(
        &mut self,
        process: ProcessId,
        channel: ChannelId,
        bytes: &[u8],
        parcel: Option<Parcel>,
    ) -> Result<Sent, Error> {
        let entry = self.channels.find(channel, &self.lineage)?;
        let owner = self.processes.find(process, &self.lineage)?;
        if owner.senders.count_of(channel) == 0 {
            return Err(Error::Refused(Refusal::NotHeld));
        }
        let len = u32::try_from(bytes.len()).map_err(|_| Error::Refused(Refusal::MessageTooLong))?;
        let frames = u64::try_from(entry.frames.len()).unwrap_or(u64::MAX);
        let admitted = entry.spec.admit(len, entry.queued(), frames, parcel.is_some()).map_err(Error::Refused)?;
        if let Some(parcel) = parcel {
            self.parcel_sendable(process, parcel)?;
        }
        if entry.receivers == 0 {
            return Ok(Sent::NoReceiver);
        }
        let Some(take) = admitted else { return Ok(Sent::Full) };
        let framed = matches!(entry.spec.framing, Framing::Messages { .. });
        // ここから下は確定。
        if let Some(parcel) = parcel {
            self.parcel_depart(process, parcel)?;
        }
        let part = usize::try_from(take).ok().and_then(|take| bytes.get(..take));
        let part = part.ok_or(Error::Broken(Invariant::ChannelKept))?;
        let entry = self.channels.get_mut(channel).ok_or(Error::Broken(Invariant::ChannelKept))?;
        entry.bytes.extend_within(part).map_err(|()| Error::Broken(Invariant::ChannelKept))?;
        if framed {
            let frame = Frame { len: take, parcel };
            entry.frames.push_back_within(frame).map_err(|_| Error::Broken(Invariant::ChannelKept))?;
        }
        self.sync_channel(channel)?;
        Ok(Sent::Accepted(u64::from(take)))
    }

    /// 中身を `buffer` の先頭へ受け取る。`process` は受ける端を保持していなければならない。
    ///
    /// メッセージの列では、先頭の一つを丸ごと受け取る。`buffer` が足りなければ、何も変えずに
    /// 拒否する (長さは [`Kernel::next_message_len`] で分かる)。流れでは、`buffer` に入るぶんを受け取る。
    pub fn receive(&mut self, process: ProcessId, channel: ChannelId, buffer: &mut [u8]) -> Result<Received, Error> {
        let entry = self.channels.find(channel, &self.lineage)?;
        let owner = self.processes.find(process, &self.lineage)?;
        if owner.receivers.count_of(channel) == 0 {
            return Err(Error::Refused(Refusal::NotHeld));
        }
        if !entry.has_content() {
            return Ok(if entry.senders == 0 { Received::Ended } else { Received::Empty });
        }
        let (room, parcel) = match entry.frames.get(0).copied() {
            Some(frame) => {
                let len = usize::try_from(frame.len).map_err(|_| Error::Broken(Invariant::ChannelKept))?;
                (buffer.get_mut(..len).ok_or(Error::Refused(Refusal::BufferTooSmall))?, frame.parcel)
            }
            None => (buffer, None),
        };
        if let Some(parcel) = parcel {
            // 受け手の保持を増やす。失敗したら何も変えていない。
            self.parcel_arrive(process, parcel)?;
        }
        let entry = self.channels.get_mut(channel).ok_or(Error::Broken(Invariant::ChannelKept))?;
        let _ = entry.frames.pop_front();
        let bytes = u64::try_from(entry.bytes.drain_into(room)).unwrap_or(u64::MAX);
        self.sync_channel(channel)?;
        Ok(Received::Data { bytes, parcel })
    }

    /// 通信路が持っているバイト数。
    pub fn channel_queued(&self, channel: ChannelId) -> Result<u64, Error> {
        Ok(self.channels.find(channel, &self.lineage)?.queued())
    }

    /// 次に受け取るメッセージの長さ。中身が無ければ `None`。流れでは、持っているバイト数。
    pub fn next_message_len(&self, channel: ChannelId) -> Result<Option<u64>, Error> {
        let entry = self.channels.find(channel, &self.lineage)?;
        Ok(match entry.frames.get(0) {
            Some(frame) => Some(u64::from(frame.len)),
            None => Some(entry.queued()).filter(|queued| *queued != 0),
        })
    }

    /// 端の保持を一つ外す。プロセス側の記録は呼び出し側が既に減らしている。確保しない。
    pub(crate) fn drop_end(&mut self, channel: ChannelId, end: End) -> Result<(), Error> {
        let entry = self.channels.get_mut(channel).ok_or(Error::Broken(Invariant::ChannelKept))?;
        let held = match end {
            End::Send => &mut entry.senders,
            End::Receive => &mut entry.receivers,
        };
        *held = held.checked_sub(1).ok_or(Error::Broken(Invariant::ChannelKept))?;
        if entry.receivers == 0 {
            // 受け取る者がいない中身は捨てる。運んでいた資源の保持も手放す。
            entry.bytes.clear();
            loop {
                let entry = self.channels.get_mut(channel).ok_or(Error::Broken(Invariant::ChannelKept))?;
                let Some(frame) = entry.frames.pop_front() else { break };
                if let Some(parcel) = frame.parcel {
                    self.parcel_drop(parcel)?;
                }
            }
        }
        let entry = self.channels.get(channel).ok_or(Error::Broken(Invariant::ChannelKept))?;
        if entry.senders != 0 || entry.receivers != 0 {
            return self.sync_channel(channel);
        }
        let (readable, writable, capacity) = (entry.readable, entry.writable, entry.capacity);
        self.channels.remove(channel).ok_or(Error::Broken(Invariant::ChannelKept))?;
        self.channel_bytes = self.channel_bytes.checked_sub(capacity).ok_or(Error::Broken(Invariant::ChannelKept))?;
        self.unpin(readable)?;
        self.unpin(writable)
    }

    /// 通知の資源の数を中身に合わせ、成立した待機を確定する。
    fn sync_channel(&mut self, channel: ChannelId) -> Result<(), Error> {
        let entry = self.channels.get(channel).ok_or(Error::Broken(Invariant::ChannelKept))?;
        let levels = [(entry.readable, entry.readable_now()), (entry.writable, entry.writable_now())];
        for (object, on) in levels {
            let entry = self.objects.get_mut(object).ok_or(Error::Broken(Invariant::ChannelKept))?;
            let next = u64::from(on);
            if entry.count != next {
                entry.count = next;
                self.settle(object)?;
            }
        }
        Ok(())
    }

    /// `process` が `parcel` を送り出せるか。受理前の検査。
    fn parcel_sendable(&self, process: ProcessId, parcel: Parcel) -> Result<(), Error> {
        let owner = self.processes.find(process, &self.lineage)?;
        let held = match parcel {
            Parcel::Object(object) => {
                self.can_pin(object, 1)?;
                owner.objects.count_of(object)
            }
            Parcel::Backing(backing) => {
                let entry = self.backings.find(backing, &self.lineage)?;
                entry.transit.checked_add(1).ok_or(Error::Exhausted(Resource::Pins))?;
                owner.backings.count_of(backing)
            }
        };
        if held == 0 { Err(Error::Refused(Refusal::NotHeld)) } else { Ok(()) }
    }

    /// `process` の保持を一つ、運ばれている状態へ移す。[`Kernel::parcel_sendable`] で確かめた後に呼ぶ。
    fn parcel_depart(&mut self, process: ProcessId, parcel: Parcel) -> Result<(), Error> {
        match parcel {
            Parcel::Object(object) => {
                self.pin(object)?;
                let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ChannelKept))?;
                owner.objects.sub(object).map_err(|_| Error::Broken(Invariant::ChannelKept))?;
                self.drop_handle(object)
            }
            Parcel::Backing(backing) => {
                let entry = self.backings.get_mut(backing).ok_or(Error::Broken(Invariant::ChannelKept))?;
                entry.transit = entry.transit.checked_add(1).ok_or(Error::Broken(Invariant::ChannelKept))?;
                let owner = self.processes.get_mut(process).ok_or(Error::Broken(Invariant::ChannelKept))?;
                owner.backings.sub(backing).map_err(|_| Error::Broken(Invariant::ChannelKept))?;
                self.drop_backing_hold(backing)
            }
        }
    }

    /// 運ばれている保持を `process` へ渡す。失敗したら何も変えない。
    fn parcel_arrive(&mut self, process: ProcessId, parcel: Parcel) -> Result<(), Error> {
        let limit = self.holdings_limit(process);
        match parcel {
            Parcel::Object(object) => {
                let handles = self.objects.get(object).ok_or(Error::Broken(Invariant::ChannelKept))?.handles;
                let handles = handles.checked_add(1).ok_or(Error::Refused(Refusal::HoldOverflow))?;
                let owner = self.alive_mut(process)?;
                owner.objects.add(object, limit)?;
                self.objects.get_mut(object).ok_or(Error::Broken(Invariant::ChannelKept))?.handles = handles;
                self.unpin(object)
            }
            Parcel::Backing(backing) => {
                let holds = self.backings.get(backing).ok_or(Error::Broken(Invariant::ChannelKept))?.holds;
                let holds = holds.checked_add(1).ok_or(Error::Refused(Refusal::HoldOverflow))?;
                let owner = self.alive_mut(process)?;
                owner.backings.add(backing, limit)?;
                let entry = self.backings.get_mut(backing).ok_or(Error::Broken(Invariant::ChannelKept))?;
                entry.holds = holds;
                entry.transit = entry.transit.checked_sub(1).ok_or(Error::Broken(Invariant::ChannelKept))?;
                Ok(())
            }
        }
    }

    /// 運ばれている保持を、誰にも渡さずに手放す。確保しない。
    fn parcel_drop(&mut self, parcel: Parcel) -> Result<(), Error> {
        match parcel {
            Parcel::Object(object) => self.unpin(object),
            Parcel::Backing(backing) => {
                let entry = self.backings.get_mut(backing).ok_or(Error::Broken(Invariant::ChannelKept))?;
                entry.transit = entry.transit.checked_sub(1).ok_or(Error::Broken(Invariant::ChannelKept))?;
                self.reclaim_backing_if_unused(backing)
            }
        }
    }
}
