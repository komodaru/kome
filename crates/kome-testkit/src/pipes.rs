//! 通信路の独立したモデル。
//!
//! kome の入れ物も判定も使わず、標準の入れ物で「何が入っていて、誰が端を持っているか」を持つ。
//! 決定的シミュレーションは、送信と受信の結果・中身・通知をこれと比べる。

use std::collections::{BTreeMap, VecDeque};

/// 通信路の形。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// メッセージの列: 持てる数と、一つの長さの上限。
    Messages(usize, usize),
    /// バイトの流れ: 持てるバイト数。
    Stream(usize),
}

/// 送信の結果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Put {
    /// 受け付けたバイト数。
    Accepted(usize),
    /// 空きが無い。
    Full,
    /// 受ける端が無い。
    NoReceiver,
    /// 一つのメッセージの上限より長い。
    TooLong,
}

/// 受信の結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Got<X> {
    /// 受け取ったバイト列と、運ばれてきた資源。
    Data(Vec<u8>, Option<X>),
    /// 中身が無い。
    Empty,
    /// 中身が無く、送る端も無い。
    Ended,
    /// 先頭のメッセージがバッファより長い。
    TooSmall,
}

/// 一つの通信路。`P` は端を持つ者、`X` は運ばれる資源。
#[derive(Clone, Debug)]
pub struct Pipe<P, X> {
    shape: Shape,
    messages: VecDeque<(Vec<u8>, Option<X>)>,
    stream: VecDeque<u8>,
    senders: BTreeMap<P, u32>,
    receivers: BTreeMap<P, u32>,
}

impl<P: Ord + Copy, X: Copy> Pipe<P, X> {
    /// `owner` が両端を一つずつ持つ、空の通信路。
    pub fn new(shape: Shape, owner: P) -> Pipe<P, X> {
        Pipe {
            shape,
            messages: VecDeque::new(),
            stream: VecDeque::new(),
            senders: BTreeMap::from([(owner, 1)]),
            receivers: BTreeMap::from([(owner, 1)]),
        }
    }

    fn side(&mut self, sending: bool) -> &mut BTreeMap<P, u32> {
        if sending { &mut self.senders } else { &mut self.receivers }
    }

    /// `who` がその端を持っているか。
    pub fn holds(&self, who: P, sending: bool) -> bool {
        if sending { self.senders.contains_key(&who) } else { self.receivers.contains_key(&who) }
    }

    /// その端を誰かが持っているか。
    pub fn open(&self, sending: bool) -> bool {
        if sending { !self.senders.is_empty() } else { !self.receivers.is_empty() }
    }

    /// 端の保持を一つ増やす。
    pub fn retain(&mut self, who: P, sending: bool) {
        let held = self.side(sending).entry(who).or_insert(0);
        *held = held.saturating_add(1);
    }

    /// 端の保持を一つ減らす。受ける端が無くなれば中身を捨て、捨てた資源を返す。
    pub fn release(&mut self, who: P, sending: bool) -> Vec<X> {
        let side = self.side(sending);
        match side.get(&who).copied() {
            Some(held) if held > 1 => {
                side.insert(who, held.saturating_sub(1));
            }
            Some(_) | None => {
                side.remove(&who);
            }
        }
        self.settle()
    }

    /// `who` の保持を全て外す (プロセスの終了)。
    pub fn forget(&mut self, who: P) -> Vec<X> {
        self.senders.remove(&who);
        self.receivers.remove(&who);
        self.settle()
    }

    fn settle(&mut self) -> Vec<X> {
        if !self.receivers.is_empty() {
            return Vec::new();
        }
        self.stream.clear();
        self.messages.drain(..).filter_map(|(_, parcel)| parcel).collect()
    }

    /// 両端とも持つ者がいないか (回収される)。
    pub fn gone(&self) -> bool {
        self.senders.is_empty() && self.receivers.is_empty()
    }

    /// 持っているバイト数。
    pub fn queued(&self) -> usize {
        self.stream.len().saturating_add(self.messages.iter().map(|(bytes, _)| bytes.len()).sum())
    }

    fn has_content(&self) -> bool {
        !self.stream.is_empty() || !self.messages.is_empty()
    }

    /// 読めるか。
    pub fn readable(&self) -> bool {
        self.has_content() || self.senders.is_empty()
    }

    /// 書けるか。
    pub fn writable(&self) -> bool {
        let room = match self.shape {
            Shape::Messages(count, _) => self.messages.len() < count,
            Shape::Stream(bytes) => self.stream.len() < bytes,
        };
        room || self.receivers.is_empty()
    }

    /// 運ばれている資源。
    pub fn parcels(&self) -> impl Iterator<Item = X> + '_ {
        self.messages.iter().filter_map(|(_, parcel)| *parcel)
    }

    /// 次に受け取るメッセージの長さ。流れでは持っているバイト数。中身が無ければ `None`。
    pub fn next_len(&self) -> Option<usize> {
        match self.messages.front() {
            Some((bytes, _)) => Some(bytes.len()),
            None => Some(self.stream.len()).filter(|len| *len != 0),
        }
    }

    /// 資源を運べる形か。
    pub fn carries(&self) -> bool {
        matches!(self.shape, Shape::Messages(..))
    }

    /// 送る。
    pub fn put(&mut self, bytes: &[u8], parcel: Option<X>) -> Put {
        match self.shape {
            Shape::Messages(_, max_len) if bytes.len() > max_len => Put::TooLong,
            Shape::Messages(..) | Shape::Stream(_) if self.receivers.is_empty() => Put::NoReceiver,
            Shape::Messages(count, _) if self.messages.len() >= count => Put::Full,
            Shape::Messages(..) => {
                self.messages.push_back((bytes.to_vec(), parcel));
                Put::Accepted(bytes.len())
            }
            Shape::Stream(limit) if self.stream.len() >= limit => Put::Full,
            Shape::Stream(limit) => {
                let take = bytes.len().min(limit.saturating_sub(self.stream.len()));
                self.stream.extend(bytes.iter().take(take));
                Put::Accepted(take)
            }
        }
    }

    /// 次に受け取るもの。取り出さない。
    pub fn peek(&self, room: usize) -> Got<X> {
        if !self.has_content() {
            return if self.senders.is_empty() { Got::Ended } else { Got::Empty };
        }
        match self.messages.front() {
            Some((bytes, _)) if bytes.len() > room => Got::TooSmall,
            Some((bytes, parcel)) => Got::Data(bytes.clone(), *parcel),
            None => Got::Data(self.stream.iter().copied().take(room).collect(), None),
        }
    }

    /// [`Pipe::peek`] が返したデータを取り除く。
    pub fn take(&mut self, bytes: usize) {
        if self.messages.pop_front().is_none() {
            self.stream.drain(..bytes.min(self.stream.len()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_keeps_boundaries_and_drops_content_without_receivers() {
        let mut pipe: Pipe<u8, u8> = Pipe::new(Shape::Messages(2, 4), 1);
        assert_eq!(pipe.put(b"abcde", None), Put::TooLong);
        assert_eq!(pipe.put(b"ab", Some(9)), Put::Accepted(2));
        assert_eq!(pipe.put(b"", None), Put::Accepted(0));
        assert_eq!(pipe.put(b"c", None), Put::Full);
        assert!(pipe.readable() && !pipe.writable());
        assert_eq!(pipe.peek(1), Got::TooSmall);
        assert_eq!(pipe.peek(2), Got::Data(b"ab".to_vec(), Some(9)));
        assert_eq!((pipe.next_len(), pipe.parcels().collect::<Vec<_>>()), (Some(2), vec![9]));
        pipe.take(2);
        assert_eq!(pipe.queued(), 0);
        assert_eq!(pipe.peek(0), Got::Data(Vec::new(), None));
        assert_eq!(pipe.put(b"x", Some(7)), Put::Accepted(1));
        assert_eq!(pipe.release(1, false), vec![7]);
        assert_eq!(pipe.put(b"x", None), Put::NoReceiver);
        assert!(pipe.writable() && !pipe.gone());
        assert!(pipe.release(1, true).is_empty());
        assert!(pipe.gone());
        let mut flow: Pipe<u8, u8> = Pipe::new(Shape::Stream(3), 1);
        assert_eq!(flow.put(b"abcd", None), Put::Accepted(3));
        assert_eq!(flow.put(b"d", None), Put::Full);
        assert_eq!(flow.peek(2), Got::Data(b"ab".to_vec(), None));
        flow.take(2);
        flow.retain(2, false);
        assert!(flow.forget(1).is_empty());
        assert_eq!(flow.peek(8), Got::Data(b"c".to_vec(), None));
        flow.take(1);
        assert_eq!(flow.peek(8), Got::Ended);
    }
}
