//! Fragmented framing: wire header, receive-side reassembly, and the
//! send-side round-robin scheduler. Shared between [`crate::client`] and
//! [`crate::server`], which differ only in which [`Kind`]s they originate
//! and dispatch.

use std::{
    collections::{HashMap, VecDeque},
    fmt, io, mem,
    sync::Arc,
    task::Poll,
};

use bytes::{Buf, BufMut, Bytes, BytesMut};

use ::serde::{Deserialize, Serialize};

use crate::{
    Error, Limits, NEGOTIATE_FRAGMENT_SIZE, NEGOTIATE_MAX_PAYLOAD_SIZE,
    auth::{self, Auth},
    session::Ledger,
    trailer::{RecvShared, SendAction, SendShared},
    transport::{
        AnyReceiver, AnySender, OutgoingHandles, ReceivedHandles, Receiver, RecvFrame, SendFrame,
        Sender,
    },
    window::{ControlSink, PayloadBudget, PayloadCharge, SessionWindow},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum Kind {
    Request = 0,
    Response = 1,
    Error = 2,
    Cancel = 3,
    /// Advisory: the sender no longer wants any more `TRAILER` fragments for
    /// the given message id. Unlike `Cancel`, this never affects the
    /// message's own request/response outcome — it only tells the peer to
    /// stop streaming a trailer it already committed to sending.
    Discard = 4,
    /// Protocol handshake/version negotiation. Sent unprompted and first by
    /// both ends of a connection before any other `Kind` is valid; see
    /// [`negotiate`].
    Negotiate = 5,
    /// Confirms receipt of the final non-trailer fragment carrying
    /// `WANT_ACK` for the message identified by `id`.
    Ack = 6,
    /// Drops references to a session opaque. `id` names the opaque rather
    /// than a message, and the 4-byte payload carries how many references
    /// are being dropped at once.
    ///
    /// This lives at transport level, not in any [`Protocol`](crate::Protocol),
    /// because [`Opaque`](crate::session::Opaque) is generic over the
    /// application protocol: releasing is the RPC runtime's business, and an
    /// application that never names an opaque still needs its peer's
    /// references collected.
    ///
    /// A release for an unknown `id` is ignored rather than treated as a
    /// protocol error. The owner may legitimately have retired the entry
    /// already — a consuming operation races the peer's release by
    /// construction, and the counters are commutative, so both orders
    /// converge.
    Release = 7,
    /// Returns trailer flow-control credit. `id` names the *message* whose
    /// trailer is being credited, and the 4-byte payload carries how many
    /// bytes the consumer has retired since the last `Credit` for that id.
    ///
    /// Credit is session-scoped: the count returns to the connection-wide
    /// pool, which is the only credit limit. The `id` is still required,
    /// because the sender keeps its pool debt keyed by message id so that a
    /// `Discard` can refund a trailer's whole remainder implicitly, and
    /// because per-trailer retirement is the signal a sender needs to tell a
    /// draining trailer from a stalled one.
    ///
    /// "Retired" means the consuming application released the credit, which
    /// is deliberately later than having read the bytes. Reading only moves
    /// data from `stage` into the consumer's buffer; it does not free it.
    ///
    /// Like [`Kind::Release`], credit for an unknown `id` is ignored rather
    /// than treated as a protocol error: the sender may legitimately have
    /// finished or aborted the trailer already, and the two race by
    /// construction.
    Credit = 8,
    /// Returns postcard payload quota. Session-scoped in a way `Kind::Credit`
    /// is not: the `id` field is unused and must be zero, and the 4-byte
    /// payload carries the total bytes released since the last one.
    ///
    /// Dropping the id is the point. Payload quota is charged per message but
    /// released per *call*, and any number of calls may retire between two
    /// turns of the writer; naming each one would put an otherwise pointless
    /// fragment on the wire per retirement. A bare count coalesces them all.
    /// Nothing needs the attribution, either: unlike a trailer sender, which
    /// keeps its pool debt keyed by id so a `Discard` can refund a whole
    /// remainder implicitly, a payload sender knows exactly what it charged
    /// each send and settles a cancelled one from its own records.
    PayloadCredit = 9,
}

impl TryFrom<u8> for Kind {
    type Error = Error;

    fn try_from(value: u8) -> Result<Self, Error> {
        match value {
            0 => Ok(Self::Request),
            1 => Ok(Self::Response),
            2 => Ok(Self::Error),
            3 => Ok(Self::Cancel),
            4 => Ok(Self::Discard),
            5 => Ok(Self::Negotiate),
            6 => Ok(Self::Ack),
            7 => Ok(Self::Release),
            8 => Ok(Self::Credit),
            9 => Ok(Self::PayloadCredit),
            _ => Err(Error::Protocol(format!("unknown frame kind {value}"))),
        }
    }
}

/// Fragment header flag bits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Flags(u8);

impl Flags {
    pub(crate) const NONE: Flags = Flags(0);
    pub(crate) const FIRST: Flags = Flags(0b0001);
    /// Last fragment of the message, whatever phase it is in.
    pub(crate) const LAST: Flags = Flags(0b0010);
    pub(crate) const ABORT: Flags = Flags(0b0100);
    /// The postcard payload ends with this fragment and trailer data
    /// follows.
    pub(crate) const TRAILER: Flags = Flags(0b1000);
    pub(crate) const WANT_ACK: Flags = Flags(0b1_0000);
    const VALID: u8 =
        Self::FIRST.0 | Self::LAST.0 | Self::ABORT.0 | Self::TRAILER.0 | Self::WANT_ACK.0;

    pub(crate) fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }

    fn bits(self) -> u8 {
        self.0
    }

    fn from_bits(bits: u8) -> Result<Self, Error> {
        if bits & !Self::VALID != 0 {
            return Err(Error::Protocol(format!("invalid fragment flags {bits:#x}")));
        }
        Ok(Flags(bits))
    }
}

impl std::ops::BitOr for Flags {
    type Output = Flags;

    fn bitor(self, rhs: Flags) -> Flags {
        Flags(self.0 | rhs.0)
    }
}

impl std::ops::BitAnd for Flags {
    type Output = Flags;

    fn bitand(self, rhs: Flags) -> Flags {
        Flags(self.0 & rhs.0)
    }
}

#[repr(C, packed)]
struct RawFragmentHeader {
    flags: [u8; 1],
    kind: [u8; 1],
    // Reserved for future use; always zero-filled on write, never validated
    // or surfaced on read (a peer sending non-zero reserved bytes today must
    // not be rejected). Fields are ordered by ascending size so each falls
    // on a naturally aligned offset (0, 1, 2, 4, 8) if ever read from an
    // aligned buffer, though decoding today is manual `from_le_bytes` and
    // doesn't rely on that.
    reserved: [u8; 2],
    payload_len: [u8; 4],
    id: [u8; 8],
}

impl RawFragmentHeader {
    const LEN: usize = size_of::<Self>();

    fn new(flags: Flags, kind: Kind, id: u64, payload_len: u32) -> Self {
        Self {
            flags: [flags.bits()],
            kind: [kind as u8],
            reserved: [0; 2],
            payload_len: payload_len.to_le_bytes(),
            id: id.to_le_bytes(),
        }
    }

    fn as_bytes(&self) -> [u8; Self::LEN] {
        // SAFETY: RawFragmentHeader is packed, contains no padding, and
        // consists only of byte arrays, so a bitwise copy of its bytes is
        // always valid.
        unsafe { mem::transmute_copy(self) }
    }

    fn decode(bytes: &[u8; Self::LEN]) -> Result<(Flags, Kind, u64, usize), Error> {
        // SAFETY: `bytes` has exactly the layout of `RawFragmentHeader`.
        let header = unsafe { &*bytes.as_ptr().cast::<Self>() };
        // Intentionally read but discarded: `reserved` is forward-compatible
        // padding, never validated or surfaced.
        let _ = header.reserved;
        Ok((
            Flags::from_bits(header.flags[0])?,
            Kind::try_from(header.kind[0])?,
            u64::from_le_bytes(header.id),
            u32::from_le_bytes(header.payload_len) as usize,
        ))
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FragmentHeader {
    pub(crate) flags: Flags,
    pub(crate) kind: Kind,
    pub(crate) id: u64,
    pub(crate) payload_len: usize,
}

impl FragmentHeader {
    pub(crate) fn encode_into(&self, buffer: &mut impl BufMut) {
        let payload_len = u32::try_from(self.payload_len).expect("fragment payload is too large");
        let raw = RawFragmentHeader::new(self.flags, self.kind, self.id, payload_len);
        buffer.put_slice(&raw.as_bytes());
    }

    pub(crate) fn encode(&self) -> Bytes {
        let mut buffer = BytesMut::with_capacity(RawFragmentHeader::LEN);
        self.encode_into(&mut buffer);
        buffer.freeze()
    }
}

/// Reads and decodes one fragment header, looping over partial reads.
pub(crate) async fn read_fragment_header<F: RecvFrame>(
    frame: &mut F,
) -> Result<FragmentHeader, Error> {
    let mut buf = [0u8; RawFragmentHeader::LEN];
    let mut filled = 0;
    while filled < buf.len() {
        let mut dest = &mut buf[filled..];
        let n = frame.recv(&mut dest).await?;
        if n == 0 {
            return Err(Error::ConnectionClosed);
        }
        filled += n;
    }
    let (flags, kind, id, payload_len) = RawFragmentHeader::decode(&buf)?;
    Ok(FragmentHeader {
        flags,
        kind,
        id,
        payload_len,
    })
}

/// Reads exactly `len` bytes directly into `dest`, appending to whatever it
/// already contains. Bounded with [`BufMut::limit`] on every call so a
/// single `recv()` can never read past `len` bytes, even if more bytes
/// (belonging to the next fragment) are already available.
async fn read_payload<F: RecvFrame>(
    frame: &mut F,
    dest: &mut BytesMut,
    len: usize,
) -> Result<(), Error> {
    let mut remaining = len;
    while remaining > 0 {
        let mut limited = (&mut *dest).limit(remaining);
        let n = frame.recv(&mut limited).await?;
        if n == 0 {
            return Err(Error::ConnectionClosed);
        }
        remaining -= n;
    }
    Ok(())
}

/// The protocol version this build speaks. `negotiate` advertises this as
/// its sole supported version; a future version bump adds another entry
/// here (and to the version-selection logic below) rather than replacing it.
const PROTOCOL_VERSION: u8 = 1;

/// Version 1's handshake payload: the limits this endpoint enforces on
/// incoming traffic, plus an optional authentication digest. `negotiate`
/// reduces each field of the local `Limits` to the minimum of the local and
/// peer values, so both ends converge on the same effective limits — one side
/// raising a limit has no effect unless the peer also raises it, and either
/// side can unilaterally cap what actually gets used on the wire.
///
/// `Debug` is implemented by hand: a derived one would print `key`, and a
/// derived digest authenticates its side as effectively as the key it came
/// from.
#[derive(Clone, Serialize, Deserialize)]
struct HandshakeV1 {
    max_fragment_size: u32,
    max_payload_size: u32,
    max_handles_per_fragment: u32,
    max_handles_per_message: u32,
    /// This endpoint's role-specific authentication digest, when a key is
    /// configured. See [`crate::auth`].
    key: Option<[u8; 32]>,
    max_concurrent_calls: u32,
    trailer_session_window: u32,
    max_outstanding_payload: u32,
}

impl fmt::Debug for HandshakeV1 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HandshakeV1")
            .field("max_fragment_size", &self.max_fragment_size)
            .field("max_payload_size", &self.max_payload_size)
            .field("max_handles_per_fragment", &self.max_handles_per_fragment)
            .field("max_handles_per_message", &self.max_handles_per_message)
            .field("key", &self.key.map(|_| "<redacted>"))
            .field("max_concurrent_calls", &self.max_concurrent_calls)
            .field("trailer_session_window", &self.trailer_session_window)
            .field("max_outstanding_payload", &self.max_outstanding_payload)
            .finish()
    }
}

impl HandshakeV1 {
    fn from_limits(limits: &Limits, auth: Option<Auth>) -> Self {
        let clamp = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
        #[cfg(unix)]
        let max_handles_per_fragment = limits
            .max_handles_per_fragment
            .min(crate::transport::unix::MAX_FDS_PER_FRAGMENT);
        #[cfg(not(unix))]
        let max_handles_per_fragment = limits.max_handles_per_fragment;
        Self {
            max_fragment_size: clamp(limits.max_fragment_size),
            max_payload_size: clamp(limits.max_payload_size),
            max_handles_per_fragment: clamp(max_handles_per_fragment),
            max_handles_per_message: clamp(limits.max_handles_per_message),
            key: auth.map(|auth| auth.advertise()),
            max_concurrent_calls: clamp(limits.max_concurrent_calls),
            trailer_session_window: clamp(limits.trailer_session_window),
            max_outstanding_payload: clamp(limits.max_outstanding_payload),
        }
    }

    /// Reduces the size/concurrency fields of `limits` to the minimum of
    /// their current value and this (peer-advertised) handshake's value.
    /// The buffering-threshold fields (`trailer_*_copy_threshold`) aren't
    /// part of the handshake at all — they only affect local behavior and
    /// don't need agreement.
    ///
    /// `trailer_credit_interval` is absent: it is local coalescing
    /// granularity, not a bound the peer relies on, so it needs no agreement.
    ///
    /// The trailer credit pool is floored at 1 afterwards. Only zero
    /// deadlocks: the sender would park before its first byte, and no credit
    /// could ever arrive because retirement requires bytes that were never
    /// sent. A pool below `max_fragment_size` is legal and merely produces
    /// short fragments.
    ///
    /// `max_payload_size` is lowered to the negotiated
    /// `max_outstanding_payload` at the very end. Every field above is minned
    /// independently, so a peer with a small quota can break the relationship
    /// between the two even when both endpoints are individually valid — and
    /// a per-message cap above the aggregate pool would make a legal single
    /// message unsendable. That is a peer decision rather than a local
    /// misconfiguration, so it is normalized rather than rejected. (The other
    /// direction is handled before the handshake is built; see `negotiate`.)
    fn clamp_limits(&self, limits: &mut Limits) {
        limits.max_fragment_size = limits
            .max_fragment_size
            .min(self.max_fragment_size as usize);
        limits.max_payload_size = limits.max_payload_size.min(self.max_payload_size as usize);
        limits.max_handles_per_fragment = limits
            .max_handles_per_fragment
            .min(self.max_handles_per_fragment as usize);
        #[cfg(unix)]
        {
            limits.max_handles_per_fragment = limits
                .max_handles_per_fragment
                .min(crate::transport::unix::MAX_FDS_PER_FRAGMENT);
        }
        limits.max_handles_per_message = limits
            .max_handles_per_message
            .min(self.max_handles_per_message as usize);
        limits.max_concurrent_calls = limits
            .max_concurrent_calls
            .min(self.max_concurrent_calls as usize);
        limits.trailer_session_window = limits
            .trailer_session_window
            .min(self.trailer_session_window as usize)
            .max(1);
        limits.max_outstanding_payload = limits
            .max_outstanding_payload
            .min(self.max_outstanding_payload as usize);
        limits.max_payload_size = limits.max_payload_size.min(limits.max_outstanding_payload);
    }
}

fn postcard_err(error: postcard::Error) -> Error {
    Error::Protocol(format!("negotiate: {error}"))
}

/// Result of a successful [`negotiate`] call.
#[derive(Debug)]
pub(crate) struct NegotiationResult {
    /// The negotiated RPC framing version. Not yet consulted by any caller
    /// (mirrors `HandshakeV1`, see its doc comment) — only version 1 exists,
    /// so there is nothing to branch on yet.
    #[allow(dead_code)]
    pub(crate) version: u8,
    /// The negotiated application-protocol name and version.
    pub(crate) app_protocol: (String, u16),
    /// The local `Limits` passed to `negotiate`, with each size/concurrency
    /// field reduced to the minimum of the local and peer values. See
    /// `HandshakeV1::clamp_limits`.
    pub(crate) limits: Limits,
}

/// The negotiate payload's outer shape: RPC-framing-version blobs (see
/// `negotiate`'s doc comment) alongside the mandatory application-protocol
/// name + supported-version list (order does not matter; the receiver sorts
/// it). Application-protocol versions are `u16` rather than `u8` since
/// application protocols are
/// expected to revise far more often than the RPC framing format, and they
/// travel in the payload rather than the 8-slot wire `id` field, so they
/// aren't bound by its capacity.
///
/// postcard serializes a struct as the plain sequence of its fields, the
/// same as a tuple of the same types in the same order — using a struct here
/// is purely for readability at the call sites and does not change the wire
/// format.
#[derive(Debug, Serialize, Deserialize)]
struct NegotiatePayload {
    version_blobs: Vec<Vec<u8>>,
    app_protocol: (String, Vec<u16>),
}

/// Performs the protocol handshake: exchanges supported-version lists and
/// per-version metadata, then returns the highest mutually supported
/// version. Must run to completion before any other `Kind` is sent or
/// accepted on `sender`/`receiver`.
///
/// The wire `id` field of a `Negotiate` fragment is repurposed to hold this
/// endpoint's zero-terminated list of supported 8-bit RPC framing version
/// numbers (at most 8 fit in the 8-byte field; order does not matter, since
/// the receiver sorts it before use). The
/// payload is a postcard-encoded [`NegotiatePayload`]: the first element is
/// a `Vec<Vec<u8>>` of one length-prefixed, version-specific blob per
/// non-zero entry in the id array, in the same order; the second is this
/// endpoint's optional application-protocol descriptor. Because postcard
/// already length-prefixes `Vec<u8>`, a receiver can decode the outer vector
/// — and so locate any entry — without knowing the schema of versions it
/// doesn't support.
///
/// Both ends select the same RPC framing version independently (the maximum
/// of the intersection of the two advertised lists), so no acknowledgement
/// round trip is needed. If there is no overlap, this sends a `FIRST|ABORT`
/// fragment as a failsafe/diagnostic signal and returns an error.
///
/// `app_protocol` is a mandatory `(name, supported versions)` pair — order
/// does not matter — for the application protocol layered on top of the RPC
/// framing — every caller has one to offer (there is no raw-RPC-only path;
/// see the [module documentation](crate::unbound)), so there is no skip/opt-out
/// case to represent. The peer's name must match exactly and there must be a
/// mutually supported version; either failure sends the `FIRST|ABORT` signal
/// and returns a distinct error from the RPC-version-mismatch case.
///
/// `auth` carries an optional pre-shared-key proof. It is expressed as the
/// digest to advertise and the digest to require, rather than as a role, which
/// keeps this function symmetric: the caller decides which direction it is
/// speaking in (see [`crate::auth`]). Verification happens after the peer's
/// handshake blob is decoded and before any result is returned, so a peer that
/// fails it never reaches a bound session.
pub(crate) async fn negotiate(
    sender: &mut AnySender,
    receiver: &mut AnyReceiver,
    limits: &Limits,
    app_protocol: (&str, &[u16]),
    auth: Option<Auth>,
) -> Result<NegotiationResult, Error> {
    // Advertise a quota that can actually carry a legal message. A
    // `max_outstanding_payload` below `max_payload_size` means a single
    // message at the per-message cap could never be admitted, so raise it
    // rather than refuse the configuration. `clamp_limits` handles the
    // mirror case, where the *peer's* quota is what breaks the relationship.
    let mut limits = *limits;
    limits.max_outstanding_payload = limits.max_outstanding_payload.max(limits.max_payload_size);
    let limits = &limits;
    let local_blob =
        postcard::to_stdvec(&HandshakeV1::from_limits(limits, auth)).map_err(postcard_err)?;
    let (local_name, local_versions) = app_protocol;
    // Callers need not pre-sort `local_versions`: sort our own copy here so
    // the `.rev().find(..)` below can cheaply pick the highest mutually
    // supported version instead of requiring an externally-maintained order.
    let mut local_versions = local_versions.to_vec();
    local_versions.sort_unstable();
    let local_app_protocol = (local_name.to_string(), local_versions.clone());
    let local_payload = NegotiatePayload {
        version_blobs: vec![local_blob],
        app_protocol: local_app_protocol,
    };
    let local_payload = postcard::to_stdvec(&local_payload).map_err(postcard_err)?;
    let mut local_id = [0u8; 8];
    local_id[0] = PROTOCOL_VERSION;

    // Drive the local write and the peer read concurrently: sequencing them
    // (write fully, then read) risks a deadlock if either side's handshake
    // payload is large enough to fill transport buffering before its peer
    // starts draining it.
    let (write_result, read_result) = tokio::join!(
        write_negotiate_message(sender, local_id, &local_payload),
        read_negotiate_message(receiver),
    );
    write_result?;
    let (peer_id, peer_payload) = read_result?;

    let peer_versions: Vec<u8> = peer_id.into_iter().take_while(|&v| v != 0).collect();
    let Some(negotiated) = [PROTOCOL_VERSION]
        .into_iter()
        .rev()
        .find(|version| peer_versions.contains(version))
    else {
        // Best-effort: the peer may already have reached the same
        // conclusion and closed its end, in which case this send fails.
        // That doesn't change what error we return here — we already know
        // why negotiation failed, and a symmetric peer that's also aborting
        // doesn't need the signal anyway.
        let _ = send_negotiate_abort(sender).await;
        return Err(Error::Protocol(
            "no mutually supported RPC protocol version".into(),
        ));
    };

    let NegotiatePayload {
        version_blobs: blobs,
        app_protocol: peer_app_protocol,
    } = postcard::from_bytes(&peer_payload).map_err(postcard_err)?;
    let index = peer_versions
        .iter()
        .position(|&version| version == negotiated)
        .expect("negotiated version was found in peer_versions");
    let blob = blobs.get(index).ok_or_else(|| {
        Error::Protocol("missing handshake payload for negotiated version".into())
    })?;
    let peer_handshake: HandshakeV1 = postcard::from_bytes(blob).map_err(postcard_err)?;

    // Before anything else the peer said is acted on. An unauthenticated peer
    // must not reach a bound session, and must not learn anything from the
    // failure beyond the fact that it failed.
    if let Err(error) = auth::verify(auth, peer_handshake.key) {
        // Best-effort; see the RPC-version-mismatch case above.
        let _ = send_negotiate_abort(sender).await;
        return Err(error);
    }

    let mut effective_limits = *limits;
    peer_handshake.clamp_limits(&mut effective_limits);

    let (peer_name, peer_app_versions) = peer_app_protocol;
    if local_name != peer_name {
        // Best-effort; see the RPC-version-mismatch case above.
        let _ = send_negotiate_abort(sender).await;
        return Err(Error::Protocol(format!(
            "mismatched application protocol: local {local_name:?}, peer {peer_name:?}"
        )));
    }
    let Some(&negotiated_app_version) = local_versions
        .iter()
        .rev()
        .find(|version| peer_app_versions.contains(version))
    else {
        let _ = send_negotiate_abort(sender).await;
        return Err(Error::Protocol(format!(
            "no mutually supported version of application protocol {local_name:?}"
        )));
    };
    let app_protocol = (local_name.to_string(), negotiated_app_version);

    Ok(NegotiationResult {
        version: negotiated,
        app_protocol,
        limits: effective_limits,
    })
}

/// Writes one `Kind::Negotiate` message, chunked into
/// `NEGOTIATE_FRAGMENT_SIZE`-bounded fragments with `FIRST`/`LAST` flags.
async fn write_negotiate_message(
    sender: &mut AnySender,
    id: [u8; 8],
    payload: &[u8],
) -> Result<(), Error> {
    let id = u64::from_le_bytes(id);
    let total = payload.len();
    let mut offset = 0;
    loop {
        let end = (offset + NEGOTIATE_FRAGMENT_SIZE).min(total);
        let chunk = &payload[offset..end];
        let first = offset == 0;
        let last = end == total;
        let mut flags = Flags::NONE;
        if first {
            flags = flags | Flags::FIRST;
        }
        if last {
            flags = flags | Flags::LAST;
        }
        let header = FragmentHeader {
            flags,
            kind: Kind::Negotiate,
            id,
            payload_len: chunk.len(),
        };
        let mut buffer = BytesMut::with_capacity(RawFragmentHeader::LEN + chunk.len());
        header.encode_into(&mut buffer);
        buffer.put_slice(chunk);
        let mut buffer = buffer.freeze();
        sender.send().finish(&mut buffer).await.map_err(Error::Io)?;
        sender.flush().await?;
        offset = end;
        if last {
            break;
        }
    }
    Ok(())
}

/// Sends the `FIRST|ABORT` no-compatible-version failsafe signal.
async fn send_negotiate_abort(sender: &mut AnySender) -> Result<(), Error> {
    let header = FragmentHeader {
        flags: Flags::FIRST | Flags::ABORT,
        kind: Kind::Negotiate,
        id: 0,
        payload_len: 0,
    };
    let mut buffer = header.encode();
    sender.send().finish(&mut buffer).await.map_err(Error::Io)?;
    sender.flush().await?;
    Ok(())
}

/// Reads one `Kind::Negotiate` message, accumulating payload bytes across
/// continuation fragments until `LAST`. Returns the peer's id-array bytes
/// and full payload. Treats a `FIRST|ABORT` fragment as the peer signaling
/// incompatible versions, surfaced as an error.
async fn read_negotiate_message(receiver: &mut AnyReceiver) -> Result<([u8; 8], Vec<u8>), Error> {
    let mut payload = BytesMut::new();
    let mut id = [0u8; 8];
    let mut started = false;
    loop {
        let mut frame = receiver.recv();
        let header = read_fragment_header(&mut frame).await?;
        if header.kind != Kind::Negotiate {
            return Err(Error::Protocol(format!(
                "expected a Negotiate frame, got {:?}",
                header.kind
            )));
        }
        let first = header.flags.contains(Flags::FIRST);
        let last = header.flags.contains(Flags::LAST);
        let abort = header.flags.contains(Flags::ABORT);
        if header.flags.contains(Flags::WANT_ACK) {
            return Err(Error::Protocol(
                "negotiate fragment cannot request an acknowledgement".into(),
            ));
        }
        if abort {
            if !first || last || header.flags.contains(Flags::TRAILER) || header.payload_len != 0 {
                return Err(Error::Protocol("invalid negotiate ABORT fragment".into()));
            }
            return Err(Error::Protocol(
                "peer aborted RPC protocol negotiation (no mutually supported version)".into(),
            ));
        }
        if header.payload_len > NEGOTIATE_FRAGMENT_SIZE {
            return Err(Error::Protocol(
                "negotiate fragment exceeds the minimum tolerated size".into(),
            ));
        }
        if payload.len() + header.payload_len > NEGOTIATE_MAX_PAYLOAD_SIZE {
            return Err(Error::Protocol(
                "negotiate message exceeds the maximum tolerated total size".into(),
            ));
        }
        if first {
            if started {
                return Err(Error::Protocol("duplicate FIRST negotiate fragment".into()));
            }
            started = true;
            id = header.id.to_le_bytes();
        } else if !started {
            return Err(Error::Protocol(
                "negotiate fragment received before FIRST".into(),
            ));
        }
        read_payload(&mut frame, &mut payload, header.payload_len).await?;
        if last {
            break;
        }
    }
    Ok((id, payload.to_vec()))
}

pub(crate) struct Message {
    pub(crate) kind: Kind,
    pub(crate) id: u64,
    pub(crate) payload: Bytes,
    pub(crate) handles: ReceivedHandles,
    pub(crate) trailer: Option<Arc<std::sync::Mutex<RecvShared>>>,
    /// This message's share of the receive-side payload quota. Travels with
    /// the message so that whatever ends up owning the call also owns the
    /// release, and no path can answer the call without dropping it.
    pub(crate) charge: PayloadCharge,
}

pub(crate) enum Event {
    None,
    /// A message opened a payload that will span further fragments, so this
    /// end is now holding a buffer for it. Reported so an endpoint can apply
    /// its own admission policy before more arrives — see
    /// [`Reassembler::payload_incomplete`], which this event announces a
    /// change to.
    PayloadIncomplete {
        id: u64,
    },
    Aborted {
        kind: Kind,
        id: u64,
        dispatched: bool,
    },
    Message(Message),
    /// The final non-trailer fragment requested an acknowledgment. `message`
    /// is present when that fragment also completed a trailerless message.
    Ack {
        id: u64,
        message: Option<Message>,
    },
    /// A trailer-data fragment. The message it belongs to was handed to the
    /// application earlier, at its payload boundary, so this never carries
    /// one.
    Trailer {
        shared: Arc<std::sync::Mutex<RecvShared>>,
        len: usize,
    },
    /// The peer released `count` bytes of payload quota, for no particular
    /// message. See [`Kind::PayloadCredit`].
    PayloadCredit {
        count: u32,
    },
    /// The peer dropped `count` references to the opaque named by `id`.
    /// Unknown ids are tolerated; see [`Kind::Release`].
    Release {
        id: u64,
        count: u32,
    },
    /// The peer retired `count` bytes of the trailer on message `id`,
    /// refunding that much of the session pool. Unknown ids are tolerated;
    /// see [`Kind::Credit`].
    Credit {
        id: u64,
        count: u32,
    },
}

struct Incomplete {
    kind: Kind,
    postcard: BytesMut,
    handles: ReceivedHandles,
    trailer: Option<Arc<std::sync::Mutex<RecvShared>>>,
    dispatched: bool,
    /// Set once the fragment carrying `TRAILER` — the payload's last — has
    /// arrived. Every fragment after it is trailer data.
    trailer_phase: bool,
    /// Set by a `WANT_ACK` fragment, which a well-behaved peer only sends
    /// once its payload is complete. Guards against a malformed peer
    /// continuing the payload past the boundary it just asked us to
    /// acknowledge, which would release its handle escrow early.
    want_ack_boundary: bool,
}

/// Reassembles postcard data while handing trailer fragments to a live
/// [`RecvShared`] without buffering their bytes.
pub(crate) struct Reassembler {
    limits: Limits,
    incomplete: HashMap<u64, Incomplete>,
    /// Number of `incomplete` entries still assembling their postcard
    /// payload, i.e. not yet in their trailer phase. Read by endpoints
    /// through [`Reassembler::payload_incomplete`].
    ///
    /// This, not `incomplete.len()`, is the counter `max_concurrent_calls` is
    /// enforced against here: a trailer-phase entry holds no payload buffer
    /// and may outlive its call, so counting it would cap long-lived trailers
    /// at the call limit.
    ///
    /// It is not the only enforcement of that limit. This check is skipped for
    /// a message that arrives whole in one fragment, which the server bounds
    /// instead in `check_call_admission` — that one counts live calls, from
    /// dispatch to response head. The client applies the same number to its
    /// own outgoing calls (`active_calls`), as flow control rather than as a
    /// protocol error.
    payload_phase: usize,
    /// Unretired trailer bytes across every open trailer, bounded by
    /// `Limits::trailer_session_window`. Shared with each trailer's
    /// `RecvShared` so credit emission and this check see one number.
    session_credit: Arc<SessionWindow>,
    /// Charged postcard bytes across every call this end has not released,
    /// bounded by `Limits::max_outstanding_payload`. Shared with the
    /// `PayloadCharge` handed out with each completed message, since a
    /// payload's charge outlives the reassembler entry that took it.
    ///
    /// A separate pool from `session_credit` on purpose: a handler that must
    /// consume a trailer before it can release its payload would deadlock
    /// against a shared one. See [`crate::window`].
    payload_credit: Arc<SessionWindow>,
    /// The route back to the peer handed to every trailer this reassembler
    /// opens and every charge it hands out, so either can credit or discard
    /// itself. Connection-scoped like the pools above, and erased to a trait
    /// object here because the reassembler is generic over no application
    /// protocol.
    sink: Arc<dyn ControlSink>,
}

impl Reassembler {
    /// Messages whose postcard payload is still arriving.
    ///
    /// Half of the concurrency an endpoint is responsible for bounding: a
    /// call is counted here until its payload completes and by the endpoint
    /// itself from then until its response. The check below bounds this
    /// count alone, which is all a reassembler can do on its own; an endpoint
    /// that knows the other half enforces the sum.
    pub(crate) fn payload_incomplete(&self) -> usize {
        self.payload_phase
    }

    /// A handle on message `id`'s share of the payload quota, releasing it
    /// when dropped.
    ///
    /// Cheap and idempotent, because the debt lives in the pool keyed by id
    /// rather than in the handle: two of these for one id release the same
    /// bytes once, not twice.
    fn charge(&self, id: u64) -> PayloadCharge {
        PayloadCharge::new(self.payload_credit.clone(), self.sink.clone(), id)
    }

    pub(crate) fn new(limits: Limits, sink: Arc<dyn ControlSink>) -> Self {
        Self {
            limits,
            incomplete: HashMap::new(),
            payload_phase: 0,
            session_credit: Arc::new(SessionWindow::new(limits.trailer_session_window)),
            payload_credit: Arc::new(SessionWindow::new(limits.max_outstanding_payload)),
            sink,
        }
    }

    pub(crate) async fn accept<F: RecvFrame>(
        &mut self,
        header: FragmentHeader,
        frame: &mut F,
    ) -> Result<Event, Error> {
        let FragmentHeader {
            flags,
            kind,
            id,
            payload_len,
        } = header;
        let first = flags.contains(Flags::FIRST);
        let last = flags.contains(Flags::LAST);
        let abort = flags.contains(Flags::ABORT);
        let trailer = flags.contains(Flags::TRAILER);
        let want_ack = flags.contains(Flags::WANT_ACK);
        #[cfg(unix)]
        let mut fragment_handles = frame.drain_fds();
        #[cfg(unix)]
        if fragment_handles.len() > self.limits.max_handles_per_fragment {
            return Err(Error::Protocol(format!(
                "fragment for message {id} exceeds the maximum native-handle count"
            )));
        }

        if kind == Kind::Ack {
            #[cfg(unix)]
            let has_handles = !fragment_handles.is_empty();
            #[cfg(not(unix))]
            let has_handles = false;
            if !first || !last || abort || trailer || want_ack || payload_len != 0 || has_handles {
                return Err(Error::Protocol("invalid Ack fragment".into()));
            }
        }

        if kind == Kind::Release {
            #[cfg(unix)]
            let has_handles = !fragment_handles.is_empty();
            #[cfg(not(unix))]
            let has_handles = false;
            if !first || !last || abort || trailer || want_ack || payload_len != 4 || has_handles {
                return Err(Error::Protocol("invalid Release fragment".into()));
            }
            let mut payload = BytesMut::with_capacity(4);
            read_payload(frame, &mut payload, 4).await?;
            let count = payload.get_u32_le();
            if count == 0 {
                return Err(Error::Protocol(
                    "Release fragment must drop at least one reference".into(),
                ));
            }
            return Ok(Event::Release { id, count });
        }

        if kind == Kind::Credit {
            #[cfg(unix)]
            let has_handles = !fragment_handles.is_empty();
            #[cfg(not(unix))]
            let has_handles = false;
            if !first || !last || abort || trailer || want_ack || payload_len != 4 || has_handles {
                return Err(Error::Protocol("invalid Credit fragment".into()));
            }
            let mut payload = BytesMut::with_capacity(4);
            read_payload(frame, &mut payload, 4).await?;
            let count = payload.get_u32_le();
            if count == 0 {
                return Err(Error::Protocol(
                    "Credit fragment must release at least one byte".into(),
                ));
            }
            return Ok(Event::Credit { id, count });
        }

        if kind == Kind::PayloadCredit {
            #[cfg(unix)]
            let has_handles = !fragment_handles.is_empty();
            #[cfg(not(unix))]
            let has_handles = false;
            // `id` is not merely unused but reserved: rejecting a nonzero one
            // keeps the field free for a future revision to give it meaning
            // without ambiguity about what an old peer put there.
            if !first || !last || abort || trailer || want_ack || payload_len != 4 || has_handles {
                return Err(Error::Protocol("invalid PayloadCredit fragment".into()));
            }
            if id != 0 {
                return Err(Error::Protocol(
                    "PayloadCredit fragment must not name a message".into(),
                ));
            }
            let mut payload = BytesMut::with_capacity(4);
            read_payload(frame, &mut payload, 4).await?;
            let count = payload.get_u32_le();
            if count == 0 {
                return Err(Error::Protocol(
                    "PayloadCredit fragment must release at least one byte".into(),
                ));
            }
            return Ok(Event::PayloadCredit { count });
        }

        if want_ack && (abort || !matches!(kind, Kind::Request | Kind::Response)) {
            return Err(Error::Protocol("invalid WANT_ACK fragment".into()));
        }

        if abort {
            if kind == Kind::Negotiate {
                return Err(Error::Auth(
                    "authentication not provided or not accepted".into(),
                ));
            }

            #[cfg(unix)]
            if !fragment_handles.is_empty() {
                return Err(Error::Protocol(
                    "ABORT fragment contains file descriptor attachments".into(),
                ));
            }
            if first || last || trailer || payload_len != 0 {
                return Err(Error::Protocol("invalid ABORT fragment".into()));
            }
            let entry = self.incomplete.remove(&id).ok_or_else(|| {
                Error::Protocol(format!("ABORT for message {id} with no active fragments"))
            })?;
            if !entry.trailer_phase {
                self.payload_phase -= 1;
            }
            if let Some(shared) = entry.trailer {
                RecvShared::fail(
                    &shared,
                    io::Error::new(io::ErrorKind::Interrupted, "trailer was aborted"),
                );
            }
            // A message cancelled before its last fragment never reached the
            // application, so no `PayloadCharge` was ever handed out to
            // release its quota. Settle it here instead: whatever the peer
            // managed to send is buffered nowhere now, and it is owed the
            // credit for it. (A message aborted *after* dispatch has a live
            // charge, and `settle` clamps to the recorded debt, so this
            // cannot double-credit either way.)
            self.charge(id).release();
            return Ok(Event::Aborted {
                kind: entry.kind,
                id,
                dispatched: entry.dispatched,
            });
        }

        if last && trailer {
            return Err(Error::Protocol(
                "TRAILER and LAST are mutually exclusive: a payload followed by a trailer has more fragments coming".into(),
            ));
        }

        // `TRAILER` rides the payload's *last* fragment, so the fragment
        // carrying it is still payload; only the ones after it are trailer
        // data. A missing entry leaves this false and is reported as such
        // by the non-FIRST branch below.
        let trailer_phase = !first
            && self
                .incomplete
                .get(&id)
                .is_some_and(|entry| entry.trailer_phase);

        #[cfg(unix)]
        if trailer_phase && !fragment_handles.is_empty() {
            return Err(Error::Protocol(
                "trailer fragment contains file descriptor attachments".into(),
            ));
        }

        if first {
            if self.incomplete.contains_key(&id) {
                return Err(Error::Protocol(format!(
                    "duplicate FIRST fragment for message {id}"
                )));
            }
            // Counts only payload-phase entries: a message in its trailer
            // phase has finished the postcard reassembly this bounds, and
            // may legitimately stay resident for the life of a long-lived
            // trailer. See `Reassembler::payload_phase`.
            if !last && self.payload_phase >= self.limits.max_concurrent_calls {
                return Err(Error::Protocol("too many incomplete messages".into()));
            }
        } else {
            let entry = self.incomplete.get(&id).ok_or_else(|| {
                Error::Protocol(format!(
                    "fragment for message {id} without an active message"
                ))
            })?;
            if entry.kind != kind {
                return Err(Error::Protocol(format!(
                    "inconsistent message kind for message {id}"
                )));
            }
            if entry.trailer_phase && trailer {
                return Err(Error::Protocol(format!(
                    "message {id} announced its payload boundary twice"
                )));
            }
            if entry.want_ack_boundary && !entry.trailer_phase {
                return Err(Error::Protocol(format!(
                    "message {id} cannot continue after a WANT_ACK boundary"
                )));
            }
        }

        if trailer_phase && last {
            // Terminal commit closing out the trailer stream. The message
            // itself went to the application back when its payload ended,
            // so there is nothing left to hand over here — even for a
            // trailer that never carried a byte, whose stream was installed
            // at that same boundary.
            if payload_len != 0 {
                return Err(Error::Protocol(
                    "trailer commit fragment must not carry a payload".into(),
                ));
            }
            let entry = self
                .incomplete
                .remove(&id)
                .expect("trailer phase implies an active message");
            let shared = entry
                .trailer
                .expect("the payload boundary installs the trailer stream");
            RecvShared::finish(&shared);
            return Ok(Event::None);
        }

        let fragment_limit = if first && last {
            self.limits.max_payload_size
        } else {
            self.limits.max_fragment_size
        };
        if payload_len > fragment_limit {
            return Err(Error::Protocol(format!(
                "fragment of {payload_len} bytes for message {id} exceeds the limit of {fragment_limit}"
            )));
        }

        if first && last {
            // Whole in one fragment: it holds no reassembly buffer, but it
            // does hold a deserialized payload for as long as the call runs,
            // which is what the quota actually bounds. Charged before the
            // bytes are read, exactly as the multi-fragment path below does.
            if !self.payload_credit.accept_bytes(id, payload_len) {
                return Err(Error::Protocol(format!(
                    "message {id} exceeded the session payload quota"
                )));
            }
            let mut payload = BytesMut::with_capacity(payload_len);
            read_payload(frame, &mut payload, payload_len).await?;
            #[cfg(unix)]
            fragment_handles.extend(frame.drain_fds());
            #[cfg(unix)]
            if fragment_handles.len() > self.limits.max_handles_per_fragment {
                return Err(Error::Protocol(format!(
                    "fragment for message {id} exceeds the maximum native-handle count"
                )));
            }
            #[cfg(unix)]
            if fragment_handles.len() > self.limits.max_handles_per_message {
                return Err(Error::Protocol(format!(
                    "message {id} exceeds the maximum native-handle count"
                )));
            }
            #[cfg(unix)]
            if !matches!(kind, Kind::Request | Kind::Response) && !fragment_handles.is_empty() {
                return Err(Error::Protocol(format!(
                    "{kind:?} fragment contains file descriptor attachments"
                )));
            }
            #[allow(unused_mut)]
            let mut handles: ReceivedHandles = Default::default();
            #[cfg(unix)]
            handles.extend(fragment_handles);
            let message = Message {
                kind,
                id,
                payload: payload.freeze(),
                handles,
                trailer: None,
                charge: self.charge(id),
            };
            return Ok(if want_ack {
                Event::Ack {
                    id,
                    message: Some(message),
                }
            } else {
                Event::Message(message)
            });
        }

        if first {
            self.incomplete.insert(
                id,
                Incomplete {
                    kind,
                    postcard: BytesMut::new(),
                    handles: Default::default(),
                    trailer: None,
                    dispatched: false,
                    trailer_phase: false,
                    want_ack_boundary: false,
                },
            );
            self.payload_phase += 1;
        }
        let entry = self.incomplete.get_mut(&id).unwrap();

        if trailer_phase {
            let shared = entry
                .trailer
                .clone()
                .expect("the payload boundary installs the trailer stream");
            // There is no cap on a trailer's total size. What bounds memory
            // is the credit the consumer has issued, so the only thing to
            // enforce here is that the peer stayed within it — both for this
            // trailer and for the session as a whole. Overrunning either is
            // connection-fatal, as a backstop against a peer that ignores
            // the windows it agreed to.
            if let Some(reason) = RecvShared::accept_bytes(&shared, payload_len) {
                return Err(Error::Protocol(format!("message {id} {reason}")));
            }
            return Ok(Event::Trailer {
                shared,
                len: payload_len,
            });
        }

        if entry.postcard.len() + payload_len > self.limits.max_payload_size {
            return Err(Error::Protocol(format!(
                "message {id} exceeds the maximum payload size"
            )));
        }
        // The aggregate bound, checked in the same breath as the per-message
        // one and fatal in the same way. A well-behaved peer parks on an
        // empty quota rather than overrunning it, so this is the backstop
        // against one that ignores the credit it was issued — there is no
        // flow control to fall back on, and the healthy path never reaches
        // it. Charged here rather than at completion because the buffer is
        // occupied from this fragment onward, not from the last one.
        if !self.payload_credit.accept_bytes(id, payload_len) {
            return Err(Error::Protocol(format!(
                "message {id} exceeded the session payload quota"
            )));
        }
        entry.postcard.reserve(payload_len);
        read_payload(frame, &mut entry.postcard, payload_len).await?;
        #[cfg(unix)]
        {
            fragment_handles.extend(frame.drain_fds());
            if fragment_handles.len() > self.limits.max_handles_per_fragment {
                return Err(Error::Protocol(format!(
                    "fragment for message {id} exceeds the maximum native-handle count"
                )));
            }
            if entry.handles.len() + fragment_handles.len() > self.limits.max_handles_per_message {
                return Err(Error::Protocol(format!(
                    "message {id} exceeds the maximum native-handle count"
                )));
            }
            if !matches!(kind, Kind::Request | Kind::Response) && !fragment_handles.is_empty() {
                return Err(Error::Protocol(format!(
                    "{kind:?} fragment contains file descriptor attachments"
                )));
            }
            entry.handles.extend(fragment_handles);
        }

        if want_ack {
            entry.want_ack_boundary = true;
        }

        if trailer {
            // The payload is complete and a trailer follows. Install the
            // trailer stream and dispatch the message now, rather than
            // waiting for the producer to emit its first fragment — a
            // producer that stalls must not hold up a payload that has
            // already arrived whole.
            let shared = RecvShared::new(
                self.limits.trailer_recv_copy_threshold,
                self.limits.trailer_recv_demand_copy_threshold,
                self.limits.trailer_credit_interval,
                self.session_credit.clone(),
                id,
                self.sink.clone(),
            );
            entry.trailer = Some(shared.clone());
            entry.trailer_phase = true;
            entry.dispatched = true;
            // Leaving payload phase: from here the entry is bounded by the
            // credit windows, not by `max_concurrent_calls`.
            self.payload_phase -= 1;
            let message = Message {
                kind,
                id,
                payload: mem::take(&mut entry.postcard).freeze(),
                handles: mem::take(&mut entry.handles),
                trailer: Some(shared),
                charge: self.charge(id),
            };
            return Ok(if want_ack {
                Event::Ack {
                    id,
                    message: Some(message),
                }
            } else {
                Event::Message(message)
            });
        }

        if last {
            let entry = self.incomplete.remove(&id).unwrap();
            self.payload_phase -= 1;
            let message = Message {
                kind,
                id,
                payload: entry.postcard.freeze(),
                handles: entry.handles,
                trailer: None,
                charge: self.charge(id),
            };
            return Ok(if want_ack {
                Event::Ack {
                    id,
                    message: Some(message),
                }
            } else {
                Event::Message(message)
            });
        }
        if want_ack {
            return Ok(Event::Ack { id, message: None });
        }
        // A payload that will span more fragments. Only its first fragment
        // announces this: a later one adds to a buffer the endpoint already
        // admitted.
        Ok(if first {
            Event::PayloadIncomplete { id }
        } else {
            Event::None
        })
    }
}

/// A message's optional trailer, as seen by the send-side scheduler.
#[derive(Clone)]
pub(crate) enum Trailer {
    None,
    Stream(std::sync::Arc<std::sync::Mutex<SendShared>>),
}

impl Trailer {
    fn is_none(&self) -> bool {
        matches!(self, Trailer::None)
    }

    fn total_len(&self) -> usize {
        match self {
            Trailer::None => 0,
            Trailer::Stream(_) => 0,
        }
    }
}

/// One outbound message the scheduler is actively (or about to be) sending.
struct ActiveSend {
    id: u64,
    kind: Kind,
    payload: Bytes,
    offset: usize,
    #[cfg(unix)]
    handles: OutgoingHandles,
    #[cfg(unix)]
    handle_offset: usize,
    trailer: Trailer,
    /// Progress through `trailer`'s bytes, once the postcard phase (`offset
    /// == payload.len()`) is done.
    /// Whether any fragment has been sent for this message yet. Distinct
    /// from `offset == 0`, which is ambiguous when `payload` is empty (a
    /// trailer-bearing message with an empty postcard phase still needs an
    /// explicit first fragment before or as part of its trailer data).
    started: bool,
    /// Whether this send occupies a concurrency slot (`payload` did not fit
    /// in one fragment at admission time, or a trailer is present — a
    /// trailer-bearing message is always at least two fragments).
    multi_fragment: bool,
    /// Session opaque references this message's payload is holding.
    ///
    /// Cleared — which commits it — the instant the payload's last fragment
    /// is written, and rescinded if the send is cancelled before then. Living
    /// on the send rather than in the endpoints is what makes that boundary
    /// unmissable: the scheduler is the only thing that knows where the
    /// payload actually ends.
    ledger: Option<Ledger>,
    /// Payload quota debited for this send when it was admitted — always the
    /// whole `payload.len()`, never a running total. Kept so a cancellation
    /// can work out how much of the charge the peer never received and hand
    /// that part straight back; zero while the send is still in `waiting`.
    charged: usize,
}

/// A control-priority item: a zero-payload `Cancel`/`Error`/`Ack`, an `ABORT`
/// for a message whose FIRST fragment already went out, a `Release`, or
/// either flavour of credit.
enum ControlSend {
    Empty { kind: Kind, id: u64 },
    Abort { id: u64 },
    Release { id: u64, count: u32 },
    Credit { id: u64, count: u32 },
    PayloadCredit { count: u32 },
}

/// Outcome of attempting to cancel an in-flight outbound send.
pub(crate) enum AbortOutcome {
    /// No trace of `id` in the scheduler — its LAST fragment already went
    /// out (or it was never admitted). The caller must fall back to the
    /// ordinary `Cancel` message flow.
    NotActive,
    /// The send was discarded before completion. `started` indicates
    /// whether any bytes (a FIRST fragment) were already sent, in which
    /// case the caller must send `ControlSend::Abort` for `id`.
    Discarded { started: bool, dispatched: bool },
}

pub(crate) enum AdvanceOutcome {
    None,
    Aborted(u64),
    #[cfg(target_os = "macos")]
    Escrow {
        id: u64,
        fds: Vec<std::os::fd::OwnedFd>,
        handles_done: bool,
    },
}

/// Send-side round-robin fragment scheduler, self-throttled so it never
/// admits more concurrently-fragmenting sends than the peer's `Reassembler`
/// is configured to track.
///
/// Constructed from the negotiated `Limits` (see `HandshakeV1::clamp_limits`),
/// which is already the minimum of the local and peer values, so throttling
/// against it here is throttling against whichever side is more
/// conservative — the peer's `Reassembler` never sees more concurrency than
/// it asked for.
pub(crate) struct Scheduler {
    active: VecDeque<ActiveSend>,
    /// Sends admitted but not yet started, because no concurrency slot or no
    /// payload quota was free at admission time. Nothing here has reached the
    /// wire or charged anything, so a send can be cancelled out of it with no
    /// trace and no settlement.
    ///
    /// Drained strictly in order (see [`Scheduler::promote_waiting`]), which
    /// is starvation-free on its own and is the whole scheduling policy under
    /// quota constraint. Anything smarter is a separate concern.
    waiting: VecDeque<ActiveSend>,
    control: VecDeque<ControlSend>,
    /// This end's remaining share of the peer's payload quota. A message is
    /// charged its whole payload before its first fragment goes out and holds
    /// it until the peer's application releases, so what is left here is what
    /// may still be started.
    payload_budget: Arc<PayloadBudget>,
    active_fragmented: usize,
    max_active_fragmented: usize,
    /// Payload budget per fragment write, already reduced from
    /// `limits.max_fragment_size` by `RawFragmentHeader::LEN` so that
    /// `limits.max_fragment_size` bounds the whole wire fragment (header +
    /// payload) actually written per round-robin turn, not just the payload.
    max_fragment_size: usize,
    #[cfg(unix)]
    max_handles_per_fragment: usize,
    /// `log2` of the current backoff factor applied to `max_fragment_size`
    /// for actual fragment writes: `effective_fragment_size() ==
    /// max_fragment_size >> fragment_shift`. Storing the shift rather than
    /// the resulting size avoids drift when `max_fragment_size` isn't a
    /// power of two — halving/doubling a stored size repeatedly wouldn't
    /// necessarily recover the exact original value.
    fragment_shift: u32,
}

/// Upper bound on `fragment_shift`, chosen to fit a 3-bit "divide by 2^n"
/// wire hint if peer-signaled throttling is added later.
const MAX_FRAGMENT_SHIFT: u32 = 7;

impl Scheduler {
    pub(crate) fn new(limits: &Limits, payload_budget: Arc<PayloadBudget>) -> Self {
        Self {
            active: VecDeque::new(),
            waiting: VecDeque::new(),
            control: VecDeque::new(),
            payload_budget,
            active_fragmented: 0,
            max_active_fragmented: limits.max_concurrent_calls.max(1),
            max_fragment_size: limits
                .max_fragment_size
                .saturating_sub(RawFragmentHeader::LEN)
                .max(1),
            #[cfg(unix)]
            max_handles_per_fragment: limits.max_handles_per_fragment,
            fragment_shift: 0,
        }
    }

    /// The fragment size to actually target for the next write, after
    /// backoff from recent short writes.
    fn effective_fragment_size(&self) -> usize {
        (self.max_fragment_size >> self.fragment_shift)
            .max(256.min(self.max_fragment_size))
            .max(1)
    }

    /// Adapts `fragment_shift` based on whether the most recent fragment
    /// write completed atomically (in a single underlying write call) or
    /// needed more than one. Backs off by one step on a short write, and
    /// decays back towards `max_fragment_size` by one step per atomic
    /// write — gradual in both directions, so a connection that's
    /// borderline doesn't flap between extremes.
    fn record_write_atomicity(&mut self, atomic: bool) {
        if atomic {
            self.fragment_shift = self.fragment_shift.saturating_sub(1);
        } else {
            self.fragment_shift = (self.fragment_shift + 1).min(MAX_FRAGMENT_SHIFT);
        }
    }

    /// Queues a payload-bearing message, starting it immediately if both a
    /// concurrency slot and its whole payload's worth of quota are free.
    ///
    /// The quota is charged in full here, before the first fragment goes out,
    /// rather than incrementally as fragments are written. That is what
    /// removes the deadlock class outright: incremental charging lets several
    /// messages reach a partially-sent state that no remaining credit can
    /// drive to completion, recoverable only by cancelling and reissuing
    /// them, while nothing releases credit until something completes. Charging
    /// at admission makes that unreachable — anything started can always be
    /// finished — and costs almost nothing in utilization, because a
    /// fully-sent message holds its whole payload against the pool anyway
    /// until the peer's application releases it. The only window where
    /// "reserved" differs from "buffered at the peer" is the transmission
    /// itself.
    ///
    /// It does not reduce interleaving either: a 16 MiB pool admits eight
    /// 2 MiB messages at once, which round-robin among themselves exactly as
    /// they would unconstrained.
    pub(crate) fn admit_message(
        &mut self,
        kind: Kind,
        id: u64,
        payload: Bytes,
        handles: OutgoingHandles,
        trailer: Trailer,
        ledger: Ledger,
    ) {
        #[cfg(unix)]
        let handles_fit = handles.fds.len() <= self.max_handles_per_fragment;
        #[cfg(not(unix))]
        let handles_fit = true;
        #[cfg(not(unix))]
        let _ = handles;
        let multi_fragment =
            !(trailer.is_none() && payload.len() <= self.max_fragment_size && handles_fit);
        let send = ActiveSend {
            id,
            kind,
            payload,
            offset: 0,
            #[cfg(unix)]
            handles,
            #[cfg(unix)]
            handle_offset: 0,
            trailer,
            started: false,
            multi_fragment,
            ledger: Some(ledger),
            charged: 0,
        };
        // A message may only jump the queue if nothing is already waiting.
        // Admission out of `waiting` is FIFO, and letting a small message
        // past a large one that is only short of quota would starve the
        // large one for as long as small ones keep arriving.
        if self.waiting.is_empty() && self.try_charge(&send) {
            self.start(send);
        } else {
            self.waiting.push_back(send);
        }
    }

    /// Takes what `send` needs to start — a concurrency slot if it needs one,
    /// and quota for its whole payload — reporting whether both were
    /// available.
    ///
    /// The quota debit happens here rather than in `start` because it is the
    /// half that can fail. It is all-or-nothing, so a `false` return has
    /// charged nothing; the slot is claimed in `start`, once both are known
    /// to be in hand.
    fn try_charge(&self, send: &ActiveSend) -> bool {
        if send.multi_fragment && self.active_fragmented >= self.max_active_fragmented {
            return false;
        }
        self.payload_budget.try_debit(send.payload.len())
    }

    /// Moves a send `try_charge` has already paid for into `active`.
    fn start(&mut self, mut send: ActiveSend) {
        send.charged = send.payload.len();
        if send.multi_fragment {
            self.active_fragmented += 1;
        }
        // Only now may its trailer producer start spending trailer credit.
        // Until a message is being driven, a trailer fragment it staged could
        // never go out — and the credit it reserved for one would be held
        // against every trailer that *could* be sent. See
        // `SendShared::started`.
        if let Trailer::Stream(shared) = &send.trailer {
            SendShared::start(shared);
        }
        self.active.push_back(send);
    }

    /// Starts as many head-of-queue waiting sends as now fit.
    ///
    /// Stops at the first one that does not, rather than looking past it for
    /// something smaller: skipping ahead is what would let a large message
    /// wait forever. Called whenever either constraint loosens — a send
    /// completing or being cancelled, or the peer returning quota.
    pub(crate) fn promote_waiting(&mut self) {
        while let Some(send) = self.waiting.front() {
            if !self.try_charge(send) {
                break;
            }
            let send = self.waiting.pop_front().expect("front was just observed");
            self.start(send);
        }
    }

    /// Admits a zero-payload control message (`Cancel`/`Error`/`Ack`), always
    /// sent as a single `FIRST|LAST` fragment ahead of ordinary sends.
    pub(crate) fn admit_empty(&mut self, kind: Kind, id: u64) {
        self.control.push_back(ControlSend::Empty { kind, id });
    }

    /// Admits an `ABORT` fragment for a message whose FIRST fragment was
    /// already sent, ahead of ordinary sends.
    pub(crate) fn admit_abort(&mut self, id: u64) {
        self.control.push_back(ControlSend::Abort { id });
    }

    /// Admits a `Release` for `count` references to the opaque `id`, ahead of
    /// ordinary sends.
    ///
    /// Ordering against ordinary sends is not a correctness requirement here:
    /// a message that *cites* an opaque holds a reference in the send escrow
    /// until its payload is fully written, so no release for a cited opaque
    /// can be admitted before the citing message's last payload fragment
    /// leaves.
    pub(crate) fn admit_release(&mut self, id: u64, count: u32) {
        debug_assert!(count > 0, "a release must drop at least one reference");
        self.control.push_back(ControlSend::Release { id, count });
    }

    /// Admits a `Credit` returning `count` retired trailer bytes for message
    /// `id`, ahead of ordinary sends.
    ///
    /// The priority matters here in a way it does not for `Release`: the peer
    /// may be parked with no credit at all, and every ordinary fragment
    /// queued ahead of this one is time it stays parked.
    pub(crate) fn admit_credit(&mut self, id: u64, count: u32) {
        debug_assert!(count > 0, "a credit must release at least one byte");
        self.control.push_back(ControlSend::Credit { id, count });
    }

    /// Admits a `PayloadCredit` returning `count` bytes of payload quota,
    /// ahead of ordinary sends and merged with any already queued.
    ///
    /// Merging is why the fragment carries no message id. Calls retire
    /// independently and often in bursts, and each one that had to name
    /// itself would cost a fragment; a bare count collapses however many
    /// retired since the writer last ran into one number. The priority is
    /// there for the same reason as `admit_credit`: the peer may be parked
    /// with nothing, and every ordinary fragment queued ahead of this is time
    /// it stays that way.
    ///
    /// There is no coalescing *threshold* to go with it. A release is already
    /// coarse — one per call, and at least a whole payload — so credit is
    /// flushed on every one, and the only batching is whatever the writer has
    /// not yet drained. A threshold would buy little and would reintroduce
    /// the class of deadlock the trailer side has to spend three force-flush
    /// clauses avoiding.
    pub(crate) fn admit_payload_credit(&mut self, count: u32) {
        debug_assert!(count > 0, "a credit must release at least one byte");
        if let Some(ControlSend::PayloadCredit { count: pending }) = self
            .control
            .iter_mut()
            .find(|item| matches!(item, ControlSend::PayloadCredit { .. }))
        {
            *pending = pending.saturating_add(count);
            return;
        }
        self.control.push_back(ControlSend::PayloadCredit { count });
    }

    /// Attempts to cancel an in-flight or not-yet-started outbound send.
    ///
    /// If the send carries a `Trailer::Stream`, its `SendShared` is put into
    /// the error state so the paired `TrailerSend`'s writer observes a clean
    /// failure instead of hanging forever waiting for a lease that will
    /// never come again (the `ActiveSend` itself, and its clone of the
    /// `Arc`, are gone after this call).
    pub(crate) fn try_cancel_active(&mut self, id: u64) -> AbortOutcome {
        if let Some(pos) = self.waiting.iter().position(|s| s.id == id) {
            let mut send = self.waiting.remove(pos).expect("position was just found");
            if let Trailer::Stream(shared) = &send.trailer {
                SendShared::discard(shared);
            }
            // Nothing of this message ever reached the wire, and nothing was
            // ever charged for it: a waiting send holds no quota, which is
            // exactly what makes charge-at-admission cheap to cancel.
            if let Some(ledger) = send.ledger.take() {
                ledger.rescind();
            }
            return AbortOutcome::Discarded {
                started: false,
                dispatched: false,
            };
        }
        if let Some(pos) = self.active.iter().position(|s| s.id == id) {
            let mut send = self.active.remove(pos).expect("position was just found");
            let started = send.started;
            let dispatched = send.ledger.is_none();
            if let Trailer::Stream(shared) = &send.trailer {
                SendShared::discard(shared);
            }
            // Present only while the payload is still incomplete: the write
            // path clears it at the payload boundary. So this rescinds
            // exactly the sends the peer cannot have decoded, and no others.
            if let Some(ledger) = send.ledger.take() {
                ledger.rescind();
            }
            // Settle the charge in two parts, because the two halves come
            // back from different places. What was earmarked at admission but
            // never transmitted is buffered nowhere and can be reclaimed
            // here, immediately. What did reach the wire is sitting in the
            // peer's reassembler, and only the peer can say when it is gone —
            // it credits that part back when it retires the aborted message.
            // Together they are exactly `charged`, with no byte counted twice
            // and none stranded.
            self.payload_budget.credit(send.charged - send.offset);
            if send.multi_fragment {
                self.active_fragmented -= 1;
            }
            self.promote_waiting();
            return AbortOutcome::Discarded {
                started,
                dispatched,
            };
        }
        AbortOutcome::NotActive
    }

    /// Handles a peer's advisory `Discard` notice: an active trailer-bearing
    /// send for `id` has its `SendShared` put into the error state (so the
    /// local producer's writer observes a clean failure), and its trailer is
    /// dropped so the send's next turn falls straight through to an
    /// ordinary zero-length `TRAILER | LAST` terminal commit — exactly as if
    /// the trailer had completed normally — rather than an `ABORT`. Unlike
    /// [`Scheduler::try_cancel_active`], this never affects the message's
    /// own request/response outcome: the postcard payload was already fully
    /// sent (and, on the peer, already dispatched) by the time a trailer can
    /// even begin, so cutting the trailer short doesn't invalidate it.
    ///
    /// A no-op if `id` has no active trailer-bearing send (it may have
    /// already finished, or the notice may have crossed on the wire with
    /// completion).
    pub(crate) fn discard_active_trailer(&mut self, id: u64) {
        if let Some(send) = self.active.iter_mut().find(|s| s.id == id)
            && let Trailer::Stream(shared) = &send.trailer
        {
            SendShared::discard(shared);
            send.trailer = Trailer::None;
        }
    }

    /// Releases the concurrency slot held by a completed multi-fragment send,
    /// then starts whatever now fits.
    ///
    /// The quota the send holds is deliberately *not* released here. Its
    /// payload is on the wire and about to be buffered by the peer, which is
    /// precisely the memory the pool is bounding; it comes back as a
    /// `PayloadCredit` once the peer's application is done with the call.
    fn free_fragmented_slot(&mut self) {
        self.active_fragmented -= 1;
        self.promote_waiting();
    }

    /// Whether the scheduler holds work it has already committed to the
    /// wire and must finish.
    ///
    /// Deliberately excludes `waiting`: a send held back for quota can only
    /// start when the peer returns credit, and credit arrives through the
    /// receive half. This is therefore the drain condition for
    /// [`Drain::Abrupt`](crate::driver::Drain::Abrupt) — the receive half is
    /// gone, no credit can arrive, and counting a waiting send would turn
    /// shutdown into a hang.
    pub(crate) fn has_work(&self) -> bool {
        !self.control.is_empty() || !self.active.is_empty()
    }

    /// Whether the scheduler holds anything at all.
    ///
    /// Counts `waiting`, so it is both the gate on polling
    /// [`Scheduler::ready`] — where a quota-blocked send registers on the
    /// pool, and which would never be armed by `has_work` alone in a session
    /// with nothing active — and the drain condition for
    /// [`Drain::Graceful`](crate::driver::Drain::Graceful), where the receive
    /// half is still running and the credit that releases a waiting send can
    /// still arrive.
    pub(crate) fn has_pending(&self) -> bool {
        self.has_work() || !self.waiting.is_empty()
    }

    /// Waits until advancing the scheduler would not block on a trailer
    /// producer or on payload quota. Once this resolves, `advance` must be
    /// driven to completion without racing ordinary message admission: it may
    /// commit part of a fragment before yielding on transport readiness.
    pub(crate) async fn ready(&mut self) {
        std::future::poll_fn(|cx| {
            if !self.control.is_empty() {
                return Poll::Ready(());
            }
            // Register on the pool *before* trying to spend it. Quota
            // returned by the peer lands there from the reader, and a credit
            // arriving between a failed debit and a later park would be lost:
            // the wake it triggered would find no one registered, and the
            // writer would park on credit it had already been given.
            if !self.waiting.is_empty() {
                self.payload_budget.park(cx.waker());
            }
            // The reader can only wake this poll, not reshuffle the queues,
            // so the promotion has to happen here for a wake-up to become
            // progress.
            self.promote_waiting();
            for send in &self.active {
                if send.offset != send.payload.len() {
                    return Poll::Ready(());
                }
                match &send.trailer {
                    Trailer::Stream(shared) if SendShared::poll_action(shared, cx).is_pending() => {
                    }
                    _ => return Poll::Ready(()),
                }
            }
            // Nothing in `active` can move and nothing waiting could start.
            // Exhausted quota is a genuine park rather than a reorder, which
            // is why `ready` may resolve to nothing here while `has_work`
            // stays true: the loop parks instead of spinning.
            Poll::Pending
        })
        .await
    }

    /// Sends one control fragment if any are queued (priority); otherwise
    /// advances the front of `active` by one fragment (postcard bytes,
    /// trailer bytes, or the trailer's terminal commit, whichever phase it's
    /// in), re-queuing it at the back if not yet complete.
    ///
    /// Reports when a streaming producer was dropped, or (on macOS) when
    /// successfully transmitted file descriptors must move into escrow.
    pub(crate) async fn advance(
        &mut self,
        transport: &mut AnySender,
    ) -> Result<AdvanceOutcome, Error> {
        if let Some(control) = self.control.pop_front() {
            self.send_control(transport, control).await?;
            return Ok(AdvanceOutcome::None);
        }
        let mut send = std::future::poll_fn(|cx| {
            let count = self.active.len();
            for _ in 0..count {
                let send = self.active.pop_front().unwrap();
                let stream_waiting = send.offset == send.payload.len()
                    && matches!(&send.trailer, Trailer::Stream(shared) if SendShared::poll_action(shared, cx).is_pending());
                if stream_waiting {
                    self.active.push_back(send);
                } else {
                    return Poll::Ready(send);
                }
            }
            Poll::Pending
        })
        .await;

        let first = !send.started;
        // A trailer-bearing message is always at least two fragments (see
        // `Reassembler`), so if its postcard phase is empty and its trailer
        // is *also* empty, an explicit (zero-length) postcard fragment must
        // still open the message before the terminal commit can follow —
        // otherwise it would have to carry FIRST, LAST, and TRAILER all at
        // once, which is rejected on receipt.
        let must_open_with_postcard = first && send.trailer.total_len() == 0;

        #[cfg(unix)]
        let handles_pending = send.handle_offset < send.handles.fds.len();
        #[cfg(not(unix))]
        let handles_pending = false;
        if send.offset < send.payload.len() || handles_pending || must_open_with_postcard {
            let start = send.offset;
            let end = (start + self.effective_fragment_size()).min(send.payload.len());
            let postcard_done = end == send.payload.len();
            #[allow(unused_mut)]
            let mut frame = transport.send();
            #[cfg(unix)]
            let attached = if handles_pending {
                let batch_end = (send.handle_offset + self.max_handles_per_fragment)
                    .min(send.handles.fds.len());
                let attached =
                    frame.attach_fds(&send.handles.fds[send.handle_offset..batch_end])?;
                if attached == 0 || attached > batch_end - send.handle_offset {
                    return Err(Error::Io(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "transport accepted an invalid native-handle batch size",
                    )));
                }
                attached
            } else {
                0
            };
            #[cfg(unix)]
            let handles_done = send.handle_offset + attached == send.handles.fds.len();
            #[cfg(not(unix))]
            let handles_done = true;
            let mut flags = Flags::NONE;
            if first {
                flags = flags | Flags::FIRST;
            }
            // The payload's final fragment always announces the boundary:
            // `LAST` ends the message outright, `TRAILER` hands off to the
            // trailer phase. Either way the peer can decode here.
            if postcard_done && handles_done {
                flags = flags
                    | if send.trailer.is_none() {
                        Flags::LAST
                    } else {
                        Flags::TRAILER
                    };
            }
            #[cfg(target_os = "macos")]
            if postcard_done && handles_done && send.handles.escrow_tracking() {
                flags = flags | Flags::WANT_ACK;
            }
            let header = FragmentHeader {
                flags,
                kind: send.kind,
                id: send.id,
                payload_len: end - start,
            };
            let mut buffer = header.encode().chain(send.payload.slice(start..end));
            let atomic = frame.finish(&mut buffer).await?;
            self.record_write_atomicity(atomic);
            if postcard_done && handles_done {
                // The payload is irrevocably on the wire, so the peer will
                // decode it and mirror every gift it carries even if it has
                // already cancelled the call. Dropping the ledger commits it;
                // a cancellation arriving from here on finds nothing left to
                // rescind, which is the intended asymmetry — a stranded
                // reference beats handing the peer a freed handle.
                if let Some(ledger) = send.ledger.take() {
                    ledger.commit();
                }
            }
            send.offset = end;
            #[cfg(target_os = "macos")]
            let escrow = send.handles.finish_attached(attached);
            #[cfg(target_os = "macos")]
            let escrow_tracking = send.handles.escrow_tracking();
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                send.handle_offset += attached;
            }
            send.started = true;
            #[cfg(target_os = "macos")]
            let id = send.id;
            if postcard_done && handles_done && send.trailer.is_none() {
                if send.multi_fragment {
                    self.free_fragmented_slot();
                }
            } else {
                self.active.push_back(send);
            }
            #[cfg(target_os = "macos")]
            if !escrow.is_empty() || escrow_tracking && handles_done {
                return Ok(AdvanceOutcome::Escrow {
                    id,
                    fds: escrow,
                    handles_done,
                });
            }
            return Ok(AdvanceOutcome::None);
        }

        if let Trailer::Stream(shared) = &send.trailer {
            match std::future::poll_fn(|cx| SendShared::poll_action(shared, cx)).await {
                SendAction::Finish => {}
                SendAction::Abort => {
                    self.control.push_back(ControlSend::Abort { id: send.id });
                    if send.multi_fragment {
                        self.free_fragmented_slot();
                    }
                    return Ok(AdvanceOutcome::Aborted(send.id));
                }
                SendAction::Fragment => {
                    debug_assert!(send.started, "trailer cannot be the first fragment");
                    let token = transport.send();
                    // SAFETY: the lease retains `token`'s mutable borrow and
                    // clears its erased representation before it ends.
                    let lease =
                        unsafe { SendShared::grant(shared, token, self.effective_fragment_size()) };
                    let (action, atomic) = SendShared::wait_fragment(shared).await?;
                    lease.complete();
                    send.started = true;
                    match action {
                        SendAction::Fragment => {
                            self.record_write_atomicity(atomic);
                            self.active.push_back(send);
                            return Ok(AdvanceOutcome::None);
                        }
                        SendAction::Finish => {}
                        SendAction::Abort => {
                            self.control.push_back(ControlSend::Abort { id: send.id });
                            if send.multi_fragment {
                                self.free_fragmented_slot();
                            }
                            return Ok(AdvanceOutcome::Aborted(send.id));
                        }
                    }
                }
            }
        }

        // Terminal commit: only reachable once both phases above are
        // exhausted, which (given `must_open_with_postcard`) implies a
        // trailer was present. The peer is already in the trailer phase —
        // the payload's last fragment carried `TRAILER` — so this only has
        // to close the message out.
        let header = FragmentHeader {
            flags: Flags::LAST,
            kind: send.kind,
            id: send.id,
            payload_len: 0,
        };
        let mut buffer = header.encode();
        transport.send().finish(&mut buffer).await?;
        if send.multi_fragment {
            self.free_fragmented_slot();
        }
        Ok(AdvanceOutcome::None)
    }

    async fn send_control(
        &mut self,
        transport: &mut AnySender,
        control: ControlSend,
    ) -> Result<(), Error> {
        let (header, count) = match control {
            ControlSend::Empty { kind, id } => (
                FragmentHeader {
                    flags: Flags::FIRST | Flags::LAST,
                    kind,
                    id,
                    payload_len: 0,
                },
                None,
            ),
            ControlSend::Abort { id } => (
                FragmentHeader {
                    flags: Flags::ABORT,
                    kind: Kind::Request,
                    id,
                    payload_len: 0,
                },
                None,
            ),
            ControlSend::Release { id, count } => (
                FragmentHeader {
                    flags: Flags::FIRST | Flags::LAST,
                    kind: Kind::Release,
                    id,
                    payload_len: 4,
                },
                Some(count),
            ),
            ControlSend::Credit { id, count } => (
                FragmentHeader {
                    flags: Flags::FIRST | Flags::LAST,
                    kind: Kind::Credit,
                    id,
                    payload_len: 4,
                },
                Some(count),
            ),
            ControlSend::PayloadCredit { count } => (
                FragmentHeader {
                    flags: Flags::FIRST | Flags::LAST,
                    kind: Kind::PayloadCredit,
                    // Reserved, and validated as zero on receipt: payload
                    // quota is released per call but returned per session.
                    id: 0,
                    payload_len: 4,
                },
                Some(count),
            ),
        };
        let mut buffer = match count {
            None => header.encode(),
            Some(count) => {
                let mut buffer = BytesMut::with_capacity(RawFragmentHeader::LEN + 4);
                header.encode_into(&mut buffer);
                buffer.put_u32_le(count);
                buffer.freeze()
            }
        };
        transport.send().finish(&mut buffer).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    /// A reassembler whose credit has nowhere to go. Every test here drives
    /// the wire format rather than the credit loop, which has its own tests
    /// in `trailer`.
    fn new_reassembler(limits: Limits) -> Reassembler {
        Reassembler::new(limits, Arc::new(NullSink))
    }

    struct NullSink;
    impl ControlSink for NullSink {
        fn credit(&self, _id: u64, _count: u32) {}
        fn payload_credit(&self, _count: u32) {}
        fn discard(&self, _id: u64) {}
    }

    /// A scheduler with the payload quota `limits` asks for. Tests that care
    /// about the quota build their own budget instead, so they can watch it.
    fn new_scheduler(limits: &Limits) -> Scheduler {
        Scheduler::new(
            limits,
            Arc::new(PayloadBudget::new(limits.max_outstanding_payload)),
        )
    }
    use std::io;
    #[cfg(unix)]
    use std::os::fd::OwnedFd;
    use std::time::Duration;

    use super::*;

    struct FakeRecvFrame {
        chunks: VecDeque<Bytes>,
    }

    impl FakeRecvFrame {
        fn new(data: impl Into<Bytes>) -> Self {
            Self {
                chunks: VecDeque::from([data.into()]),
            }
        }

        fn chunked(pieces: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: pieces.into_iter().map(Bytes::from).collect(),
            }
        }
    }

    impl RecvFrame for FakeRecvFrame {
        #[cfg(unix)]
        fn drain_fds(&mut self) -> Vec<OwnedFd> {
            Vec::new()
        }
        async fn recv<B: BufMut>(&mut self, buffer: &mut B) -> io::Result<usize> {
            let Some(front) = self.chunks.front_mut() else {
                return Ok(0);
            };
            let n = front.len().min(buffer.remaining_mut());
            if n == 0 {
                return Ok(0);
            }
            buffer.put_slice(&front[..n]);
            front.advance(n);
            if front.is_empty() {
                self.chunks.pop_front();
            }
            Ok(n)
        }
    }

    fn fast_path_bytes(id: u64, kind: Kind, payload: &[u8]) -> Vec<u8> {
        let header = FragmentHeader {
            flags: Flags::FIRST | Flags::LAST,
            kind,
            id,
            payload_len: payload.len(),
        };
        let mut bytes = header.encode().to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    fn fragment_bytes(flags: Flags, id: u64, kind: Kind, payload: &[u8]) -> Vec<u8> {
        let header = FragmentHeader {
            flags,
            kind,
            id,
            payload_len: payload.len(),
        };
        let mut bytes = header.encode().to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    /// Reads exactly `len` trailer-fragment bytes directly off `frame`,
    /// bypassing `RecvShared` (which is exercised separately in
    /// `trailer.rs`'s own unit tests). Needed between fragments so a test's
    /// next `read_fragment_header` call doesn't misread leftover payload
    /// bytes as header bytes.
    async fn drain_trailer_bytes(frame: &mut FakeRecvFrame, len: usize) -> Bytes {
        let mut buf = BytesMut::with_capacity(len);
        read_payload(frame, &mut buf, len).await.unwrap();
        buf.freeze()
    }

    #[test]
    fn hardened_defaults_bound_calls_and_native_handles() {
        let limits = Limits::default();
        // A count rather than a memory bound: what stops a peer turning this
        // into gigabytes of reassembly is `max_outstanding_payload`, which is
        // why the count can afford to be generous.
        assert_eq!(limits.max_concurrent_calls, 1024);
        assert_eq!(limits.max_outstanding_payload, 16 * 1024 * 1024);
        assert_eq!(limits.max_handles_per_fragment, 8);
        assert_eq!(limits.max_handles_per_message, 8);
    }

    #[test]
    fn raw_fragment_header_len_is_16_bytes() {
        assert_eq!(RawFragmentHeader::LEN, 16);
    }

    #[test]
    fn raw_fragment_header_round_trips_with_reserved_bytes_ignored() {
        let header = FragmentHeader {
            flags: Flags::FIRST | Flags::LAST,
            kind: Kind::Request,
            id: 0x0102_0304_0506_0708,
            payload_len: 42,
        };
        let mut bytes = header.encode().to_vec();
        // Corrupt the reserved bytes (offset 2..4) with a non-zero pattern;
        // decode must still succeed and ignore them.
        bytes[2] = 0xAA;
        bytes[3] = 0xBB;
        let bytes: [u8; RawFragmentHeader::LEN] = bytes.try_into().unwrap();
        let (flags, kind, id, payload_len) = RawFragmentHeader::decode(&bytes).unwrap();
        assert_eq!(flags, header.flags);
        assert_eq!(kind, header.kind);
        assert_eq!(id, header.id);
        assert_eq!(payload_len, header.payload_len);
    }

    #[tokio::test]
    async fn ack_is_a_single_empty_message() {
        let mut reassembler = new_reassembler(Limits::default());
        let mut frame = FakeRecvFrame::new(Bytes::new());
        let event = reassembler
            .accept(
                FragmentHeader {
                    flags: Flags::FIRST | Flags::LAST,
                    kind: Kind::Ack,
                    id: 42,
                    payload_len: 0,
                },
                &mut frame,
            )
            .await
            .unwrap();
        let Event::Message(message) = event else {
            panic!("Ack did not produce a message");
        };
        assert_eq!(message.kind, Kind::Ack);
        assert_eq!(message.id, 42);
        assert!(message.payload.is_empty());
    }

    #[tokio::test]
    async fn rejects_malformed_ack() {
        for (flags, payload_len) in [
            (Flags::FIRST, 0),
            (Flags::LAST, 0),
            (Flags::FIRST | Flags::LAST | Flags::TRAILER, 0),
            (Flags::ABORT, 0),
            (Flags::FIRST | Flags::LAST, 1),
            (Flags::FIRST | Flags::LAST | Flags::WANT_ACK, 0),
        ] {
            let mut reassembler = new_reassembler(Limits::default());
            let mut frame = FakeRecvFrame::new(Bytes::new());
            assert!(matches!(
                reassembler
                    .accept(
                        FragmentHeader {
                            flags,
                            kind: Kind::Ack,
                            id: 1,
                            payload_len,
                        },
                        &mut frame,
                    )
                    .await,
                Err(Error::Protocol(_))
            ));
        }
    }

    #[tokio::test]
    async fn last_want_ack_emits_ack_and_completed_message() {
        let mut frame = FakeRecvFrame::new(fragment_bytes(
            Flags::FIRST | Flags::LAST | Flags::WANT_ACK,
            7,
            Kind::Response,
            b"done",
        ));
        let mut reassembler = new_reassembler(Limits::default());
        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Ack {
            id,
            message: Some(message),
        } = reassembler.accept(header, &mut frame).await.unwrap()
        else {
            panic!("WANT_ACK and LAST must emit both results");
        };
        assert_eq!(id, 7);
        assert_eq!(&message.payload[..], b"done");
    }

    #[tokio::test]
    async fn want_ack_and_trailer_share_the_payload_boundary() {
        // Both flags mark "the payload is complete", so a message that has
        // attachments to escrow *and* a trailer carries them on the same
        // fragment. That fragment acknowledges and dispatches at once.
        let mut frame = FakeRecvFrame::new(fragment_bytes(
            Flags::FIRST | Flags::TRAILER | Flags::WANT_ACK,
            7,
            Kind::Response,
            b"done",
        ));
        let mut reassembler = new_reassembler(Limits::default());
        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Ack {
            id: 7,
            message: Some(message),
        } = reassembler.accept(header, &mut frame).await.unwrap()
        else {
            panic!("expected the payload boundary to ack and dispatch together");
        };
        assert_eq!(&message.payload[..], b"done");
        assert!(message.trailer.is_some());

        let mut frame = FakeRecvFrame::new(fragment_bytes(Flags::LAST, 7, Kind::Response, b""));
        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await.unwrap(),
            Event::None
        ));
    }

    #[tokio::test]
    async fn rejects_a_second_postcard_fragment_after_want_ack() {
        let mut frame = FakeRecvFrame::new(fragment_bytes(
            Flags::FIRST | Flags::WANT_ACK,
            7,
            Kind::Request,
            b"done",
        ));
        let mut reassembler = new_reassembler(Limits::default());
        let header = read_fragment_header(&mut frame).await.unwrap();
        reassembler.accept(header, &mut frame).await.unwrap();

        let mut frame = FakeRecvFrame::new(fragment_bytes(Flags::LAST, 7, Kind::Request, b"more"));
        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await,
            Err(Error::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn first_last_fragment_is_the_fast_path_and_bypasses_incomplete_bookkeeping() {
        let mut frame = FakeRecvFrame::new(fast_path_bytes(1, Kind::Request, b"hello"));
        let mut reassembler = new_reassembler(Limits::default());
        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Message(msg) = reassembler.accept(header, &mut frame).await.unwrap() else {
            panic!("fast path completes immediately");
        };
        assert_eq!(msg.kind, Kind::Request);
        assert_eq!(&msg.payload[..], b"hello");
        assert!(msg.trailer.is_none());
        assert_eq!(reassembler.incomplete.len(), 0);
    }

    #[tokio::test]
    async fn continuation_fragments_append_directly_into_the_same_buffer() {
        let mut bytes = fragment_bytes(Flags::FIRST, 1, Kind::Request, b"hello, ");
        bytes.extend(fragment_bytes(Flags::NONE, 1, Kind::Request, b"world"));
        bytes.extend(fragment_bytes(Flags::LAST, 1, Kind::Request, b"!"));
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(Limits::default());

        // Only the opening fragment announces the message; the one after it
        // adds to a buffer already accounted for.
        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await.unwrap(),
            Event::PayloadIncomplete { id: 1 }
        ));
        assert_eq!(reassembler.payload_incomplete(), 1);
        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await.unwrap(),
            Event::None
        ));
        assert_eq!(reassembler.payload_incomplete(), 1);
        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Message(msg) = reassembler.accept(header, &mut frame).await.unwrap() else {
            panic!("LAST completes the message");
        };
        assert_eq!(&msg.payload[..], b"hello, world!");
        assert_eq!(
            reassembler.payload_incomplete(),
            0,
            "the completed message hands its accounting to the endpoint"
        );
    }

    #[tokio::test]
    async fn payload_read_never_overreads_past_declared_fragment_length() {
        let mut bytes = fragment_bytes(Flags::FIRST | Flags::LAST, 1, Kind::Request, b"one");
        bytes.extend(fragment_bytes(
            Flags::FIRST | Flags::LAST,
            2,
            Kind::Request,
            b"two",
        ));
        // Deliver everything in one chunk so a single `recv()` call could
        // observe bytes belonging to the second fragment while reading the
        // first, if the read weren't correctly bounded.
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(Limits::default());

        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Message(msg) = reassembler.accept(header, &mut frame).await.unwrap() else {
            panic!("expected a completed message");
        };
        assert_eq!(&msg.payload[..], b"one");

        let header = read_fragment_header(&mut frame).await.unwrap();
        assert_eq!(header.id, 2);
        let Event::Message(msg) = reassembler.accept(header, &mut frame).await.unwrap() else {
            panic!("expected a completed message");
        };
        assert_eq!(&msg.payload[..], b"two");
    }

    #[tokio::test]
    async fn payload_read_handles_partial_chunked_delivery() {
        let bytes = fragment_bytes(Flags::FIRST | Flags::LAST, 1, Kind::Request, b"hello");
        // Split the wire bytes into single-byte chunks to force many partial
        // `recv()` calls across both the header and payload reads.
        let pieces = bytes.into_iter().map(|b| vec![b]).collect();
        let mut frame = FakeRecvFrame::chunked(pieces);
        let mut reassembler = new_reassembler(Limits::default());
        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Message(msg) = reassembler.accept(header, &mut frame).await.unwrap() else {
            panic!("expected a completed message");
        };
        assert_eq!(&msg.payload[..], b"hello");
    }

    #[tokio::test]
    async fn rejects_duplicate_first_fragment() {
        let mut frame = FakeRecvFrame::new(fragment_bytes(Flags::FIRST, 1, Kind::Request, b"a"));
        let mut reassembler = new_reassembler(Limits::default());
        let header = read_fragment_header(&mut frame).await.unwrap();
        reassembler.accept(header, &mut frame).await.unwrap();

        let mut frame = FakeRecvFrame::new(fragment_bytes(Flags::FIRST, 1, Kind::Request, b"b"));
        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await,
            Err(Error::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn rejects_continuation_without_active_message() {
        let mut frame = FakeRecvFrame::new(fragment_bytes(Flags::NONE, 1, Kind::Request, b"a"));
        let mut reassembler = new_reassembler(Limits::default());
        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await,
            Err(Error::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn rejects_fragment_after_terminal_fragment() {
        let mut bytes = fragment_bytes(Flags::FIRST | Flags::LAST, 1, Kind::Request, b"a");
        bytes.extend(fragment_bytes(Flags::NONE, 1, Kind::Request, b"b"));
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(Limits::default());
        let header = read_fragment_header(&mut frame).await.unwrap();
        reassembler.accept(header, &mut frame).await.unwrap();
        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await,
            Err(Error::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn rejects_inconsistent_kind_in_continuation() {
        let mut bytes = fragment_bytes(Flags::FIRST, 1, Kind::Request, b"a");
        bytes.extend(fragment_bytes(Flags::LAST, 1, Kind::Response, b"b"));
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(Limits::default());
        let header = read_fragment_header(&mut frame).await.unwrap();
        reassembler.accept(header, &mut frame).await.unwrap();
        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await,
            Err(Error::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn rejects_fragment_exceeding_max_fragment_size() {
        let limits = Limits {
            max_fragment_size: 4,
            ..Limits::default()
        };
        let mut frame =
            FakeRecvFrame::new(fragment_bytes(Flags::FIRST, 1, Kind::Request, b"hello"));
        let mut reassembler = new_reassembler(limits);
        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await,
            Err(Error::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn rejects_single_fragment_exceeding_max_payload_size() {
        let limits = Limits {
            max_payload_size: 4,
            ..Limits::default()
        };
        let mut frame = FakeRecvFrame::new(fast_path_bytes(1, Kind::Request, b"hello"));
        let mut reassembler = new_reassembler(limits);
        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await,
            Err(Error::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn rejects_reassembled_message_exceeding_max_payload_size() {
        let limits = Limits {
            max_fragment_size: 4,
            max_payload_size: 6,
            ..Limits::default()
        };
        let mut bytes = fragment_bytes(Flags::FIRST, 1, Kind::Request, b"abcd");
        bytes.extend(fragment_bytes(Flags::LAST, 1, Kind::Request, b"abcd"));
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(limits);
        let header = read_fragment_header(&mut frame).await.unwrap();
        reassembler.accept(header, &mut frame).await.unwrap();
        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await,
            Err(Error::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn rejects_too_many_incomplete_messages() {
        let limits = Limits {
            max_concurrent_calls: 1,
            ..Limits::default()
        };
        let mut reassembler = new_reassembler(limits);
        let mut frame = FakeRecvFrame::new(fragment_bytes(Flags::FIRST, 1, Kind::Request, b"a"));
        let header = read_fragment_header(&mut frame).await.unwrap();
        reassembler.accept(header, &mut frame).await.unwrap();

        let mut frame = FakeRecvFrame::new(fragment_bytes(Flags::FIRST, 2, Kind::Request, b"a"));
        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await,
            Err(Error::Protocol(_))
        ));
    }

    /// A message that has entered its trailer phase is done with postcard
    /// reassembly and may stay resident for as long as its trailer streams,
    /// so it must not hold a concurrency slot. If it did, a handful of
    /// long-lived trailers would make every later message a fatal protocol
    /// error. Trailer memory is bounded by the credit windows instead.
    #[tokio::test]
    async fn trailer_phase_messages_do_not_hold_concurrency_slots() {
        let limits = Limits {
            max_concurrent_calls: 1,
            ..Limits::default()
        };
        let mut reassembler = new_reassembler(limits);
        for id in 1..=3 {
            let mut frame = FakeRecvFrame::new(fragment_bytes(
                Flags::FIRST | Flags::TRAILER,
                id,
                Kind::Request,
                b"a",
            ));
            let header = read_fragment_header(&mut frame).await.unwrap();
            assert!(matches!(
                reassembler.accept(header, &mut frame).await.unwrap(),
                Event::Message(_)
            ));
        }
        assert_eq!(reassembler.payload_phase, 0);
        assert_eq!(reassembler.incomplete.len(), 3);

        // The budget is still fully available to an ordinary payload-phase
        // message even with three trailers open.
        let mut frame = FakeRecvFrame::new(fragment_bytes(Flags::FIRST, 4, Kind::Request, b"a"));
        let header = read_fragment_header(&mut frame).await.unwrap();
        reassembler.accept(header, &mut frame).await.unwrap();
        assert_eq!(reassembler.payload_phase, 1);
    }

    #[tokio::test]
    async fn rejects_nonzero_payload_on_abort() {
        let mut bytes = fragment_bytes(Flags::FIRST, 1, Kind::Request, b"a");
        bytes.extend(fragment_bytes(Flags::ABORT, 1, Kind::Request, b"x"));
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(Limits::default());
        let header = read_fragment_header(&mut frame).await.unwrap();
        reassembler.accept(header, &mut frame).await.unwrap();
        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await,
            Err(Error::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn abort_discards_accumulated_buffer_without_completing() {
        let mut bytes = fragment_bytes(Flags::FIRST, 1, Kind::Request, b"abcd");
        bytes.extend(fragment_bytes(Flags::ABORT, 1, Kind::Request, b""));
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(Limits::default());
        let header = read_fragment_header(&mut frame).await.unwrap();
        reassembler.accept(header, &mut frame).await.unwrap();
        let header = read_fragment_header(&mut frame).await.unwrap();
        let event = reassembler.accept(header, &mut frame).await.unwrap();
        assert!(matches!(
            event,
            Event::Aborted {
                dispatched: false,
                ..
            }
        ));
        assert_eq!(reassembler.incomplete.len(), 0);
    }

    #[tokio::test]
    async fn rejects_abort_for_unknown_message() {
        let mut frame = FakeRecvFrame::new(fragment_bytes(Flags::ABORT, 1, Kind::Request, b""));
        let mut reassembler = new_reassembler(Limits::default());
        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await,
            Err(Error::Protocol(_))
        ));
    }

    // --- Trailer reassembly tests ---

    #[tokio::test]
    async fn message_without_any_trailer_fragment_has_no_trailer() {
        let mut frame = FakeRecvFrame::new(fast_path_bytes(1, Kind::Request, b"hello"));
        let mut reassembler = new_reassembler(Limits::default());
        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Message(msg) = reassembler.accept(header, &mut frame).await.unwrap() else {
            panic!("expected a completed message");
        };
        assert_eq!(&msg.payload[..], b"hello");
        assert!(msg.trailer.is_none());
    }

    #[tokio::test]
    async fn present_but_empty_trailer_is_distinguishable_from_absent() {
        let mut bytes = fragment_bytes(Flags::FIRST | Flags::TRAILER, 1, Kind::Request, b"hello");
        bytes.extend(fragment_bytes(Flags::LAST, 1, Kind::Request, b""));
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(Limits::default());
        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Message(msg) = reassembler.accept(header, &mut frame).await.unwrap() else {
            panic!("the payload boundary completes the message");
        };
        assert_eq!(&msg.payload[..], b"hello");
        assert!(
            msg.trailer.is_some(),
            "TRAILER was seen, even though no trailer data ever followed"
        );
        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await.unwrap(),
            Event::None
        ));
    }

    #[tokio::test]
    async fn single_fragment_trailer_reassembles_with_postcard_payload() {
        let mut bytes = fragment_bytes(Flags::FIRST | Flags::TRAILER, 1, Kind::Request, b"hello");
        bytes.extend(fragment_bytes(Flags::NONE, 1, Kind::Request, b"world"));
        bytes.extend(fragment_bytes(Flags::LAST, 1, Kind::Request, b""));
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(Limits::default());

        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Message(msg) = reassembler.accept(header, &mut frame).await.unwrap() else {
            panic!("expected the payload boundary to dispatch the message");
        };
        assert_eq!(&msg.payload[..], b"hello");

        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Trailer { len, .. } = reassembler.accept(header, &mut frame).await.unwrap()
        else {
            panic!("expected a trailer-data fragment");
        };
        assert_eq!(&drain_trailer_bytes(&mut frame, len).await[..], b"world");

        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await.unwrap(),
            Event::None
        ));
    }

    #[tokio::test]
    async fn multi_fragment_trailer_reassembles_with_empty_postcard_payload() {
        let mut bytes = fragment_bytes(Flags::FIRST | Flags::TRAILER, 1, Kind::Request, b"");
        bytes.extend(fragment_bytes(Flags::NONE, 1, Kind::Request, b"ab"));
        bytes.extend(fragment_bytes(Flags::NONE, 1, Kind::Request, b"cd"));
        bytes.extend(fragment_bytes(Flags::LAST, 1, Kind::Request, b""));
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(Limits::default());

        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Message(msg) = reassembler.accept(header, &mut frame).await.unwrap() else {
            panic!("expected the payload boundary to dispatch the message");
        };
        assert_eq!(&msg.payload[..], b"");

        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Trailer { len, .. } = reassembler.accept(header, &mut frame).await.unwrap()
        else {
            panic!("expected the first trailer-data fragment");
        };
        assert_eq!(&drain_trailer_bytes(&mut frame, len).await[..], b"ab");

        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Trailer { len, .. } = reassembler.accept(header, &mut frame).await.unwrap()
        else {
            panic!("expected a subsequent trailer-data fragment");
        };
        assert_eq!(&drain_trailer_bytes(&mut frame, len).await[..], b"cd");

        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await.unwrap(),
            Event::None
        ));
    }

    #[tokio::test]
    async fn rejects_trailer_last_commit_with_nonzero_payload() {
        let mut bytes = fragment_bytes(Flags::FIRST | Flags::TRAILER, 1, Kind::Request, b"");
        bytes.extend(fragment_bytes(Flags::NONE, 1, Kind::Request, b"a"));
        bytes.extend(fragment_bytes(Flags::LAST, 1, Kind::Request, b"x"));
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(Limits::default());

        let header = read_fragment_header(&mut frame).await.unwrap();
        reassembler.accept(header, &mut frame).await.unwrap();

        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Trailer { len, .. } = reassembler.accept(header, &mut frame).await.unwrap()
        else {
            panic!("expected a TRAILER data event");
        };
        drain_trailer_bytes(&mut frame, len).await;

        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await,
            Err(Error::Protocol(_))
        ));
    }

    #[tokio::test]
    async fn payload_boundary_dispatches_before_any_trailer_data_arrives() {
        let mut bytes = fragment_bytes(Flags::FIRST | Flags::TRAILER, 1, Kind::Request, b"");
        bytes.extend(fragment_bytes(Flags::NONE, 1, Kind::Request, b"ab"));
        bytes.extend(fragment_bytes(Flags::LAST, 1, Kind::Request, b""));
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(Limits::default());

        // The whole point of putting TRAILER on the payload's last fragment:
        // the message is available without waiting on the trailer producer.
        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Message(msg) = reassembler.accept(header, &mut frame).await.unwrap() else {
            panic!("expected FIRST|TRAILER to dispatch the message immediately");
        };
        assert_eq!(&msg.payload[..], b"");
        assert!(msg.trailer.is_some());

        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Trailer { len, .. } = reassembler.accept(header, &mut frame).await.unwrap()
        else {
            panic!("expected a trailer-data fragment");
        };
        assert_eq!(&drain_trailer_bytes(&mut frame, len).await[..], b"ab");

        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await.unwrap(),
            Event::None
        ));
    }

    #[tokio::test]
    async fn rejects_last_and_trailer_together() {
        // Mutually exclusive: TRAILER promises more fragments, LAST denies it.
        for flags in [
            Flags::FIRST | Flags::LAST | Flags::TRAILER,
            Flags::LAST | Flags::TRAILER,
        ] {
            let mut frame = FakeRecvFrame::new(fragment_bytes(flags, 1, Kind::Request, b""));
            let mut reassembler = new_reassembler(Limits::default());
            let header = read_fragment_header(&mut frame).await.unwrap();
            assert!(matches!(
                reassembler.accept(header, &mut frame).await,
                Err(Error::Protocol(_))
            ));
        }
    }

    #[tokio::test]
    async fn rejects_a_second_payload_boundary() {
        let mut bytes = fragment_bytes(Flags::FIRST | Flags::TRAILER, 1, Kind::Request, b"");
        bytes.extend(fragment_bytes(Flags::NONE, 1, Kind::Request, b"a"));
        bytes.extend(fragment_bytes(Flags::TRAILER, 1, Kind::Request, b"b"));
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(Limits::default());

        let header = read_fragment_header(&mut frame).await.unwrap();
        reassembler.accept(header, &mut frame).await.unwrap();

        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Trailer { len, .. } = reassembler.accept(header, &mut frame).await.unwrap()
        else {
            panic!("expected a trailer-data fragment");
        };
        drain_trailer_bytes(&mut frame, len).await;

        let header = read_fragment_header(&mut frame).await.unwrap();
        assert!(matches!(
            reassembler.accept(header, &mut frame).await,
            Err(Error::Protocol(_))
        ));
    }

    /// The postcard payload and the trailer draw on separate budgets: a
    /// payload larger than the whole credit pool is fine, because the pool
    /// bounds only unretired trailer bytes.
    #[tokio::test]
    async fn postcard_and_trailer_credit_are_independent_budgets() {
        let limits = Limits {
            max_fragment_size: 4,
            max_payload_size: 8,
            trailer_session_window: 3,
            ..Limits::default()
        };
        let mut bytes = fragment_bytes(Flags::FIRST, 1, Kind::Request, b"abcd");
        bytes.extend(fragment_bytes(Flags::TRAILER, 1, Kind::Request, b"abcd"));
        bytes.extend(fragment_bytes(Flags::NONE, 1, Kind::Request, b"ab"));
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(limits);
        let header = read_fragment_header(&mut frame).await.unwrap();
        reassembler.accept(header, &mut frame).await.unwrap();
        let header = read_fragment_header(&mut frame).await.unwrap();
        reassembler.accept(header, &mut frame).await.unwrap();
        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Trailer { len, .. } = reassembler.accept(header, &mut frame).await.unwrap()
        else {
            panic!("expected a trailer-data fragment, not rejection");
        };
        drain_trailer_bytes(&mut frame, len).await;
    }

    /// The backstop against a peer that ignores the credit it was granted.
    /// A well-behaved sender parks instead of overrunning, so this is
    /// connection-fatal rather than a per-message failure. Two trailers are
    /// used because the pool is a session-wide bound: what makes the memory
    /// bound hold is that it is independent of how many trailers are open.
    #[tokio::test]
    async fn rejects_trailers_exceeding_session_window() {
        let limits = Limits {
            max_fragment_size: 8,
            trailer_session_window: 6,
            ..Limits::default()
        };
        let mut bytes = fragment_bytes(Flags::FIRST | Flags::TRAILER, 1, Kind::Request, b"");
        bytes.extend(fragment_bytes(
            Flags::FIRST | Flags::TRAILER,
            2,
            Kind::Request,
            b"",
        ));
        bytes.extend(fragment_bytes(Flags::NONE, 1, Kind::Request, b"abcd"));
        bytes.extend(fragment_bytes(Flags::NONE, 2, Kind::Request, b"abcd"));
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(limits);
        for _ in 0..2 {
            let header = read_fragment_header(&mut frame).await.unwrap();
            reassembler.accept(header, &mut frame).await.unwrap();
        }
        // Trailer 1 fits within the pool.
        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Trailer { len, .. } = reassembler.accept(header, &mut frame).await.unwrap()
        else {
            panic!("expected a trailer-data fragment");
        };
        drain_trailer_bytes(&mut frame, len).await;
        // Trailer 2 does not fit what is left of the pool.
        let header = read_fragment_header(&mut frame).await.unwrap();
        let Err(error) = reassembler.accept(header, &mut frame).await else {
            panic!("expected the overrun to be rejected");
        };
        assert!(
            format!("{error}").contains("session trailer credit window"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn abort_during_trailer_phase_discards_both_buffers() {
        let mut bytes = fragment_bytes(Flags::FIRST | Flags::TRAILER, 1, Kind::Request, b"ab");
        bytes.extend(fragment_bytes(Flags::NONE, 1, Kind::Request, b"cd"));
        bytes.extend(fragment_bytes(Flags::ABORT, 1, Kind::Request, b""));
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(Limits::default());

        let header = read_fragment_header(&mut frame).await.unwrap();
        reassembler.accept(header, &mut frame).await.unwrap();

        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Trailer { len, .. } = reassembler.accept(header, &mut frame).await.unwrap()
        else {
            panic!("expected a trailer-data fragment");
        };
        drain_trailer_bytes(&mut frame, len).await;
        assert_eq!(reassembler.incomplete.len(), 1);

        let header = read_fragment_header(&mut frame).await.unwrap();
        let event = reassembler.accept(header, &mut frame).await.unwrap();
        assert!(matches!(
            event,
            Event::Aborted {
                dispatched: true,
                ..
            }
        ));
        assert_eq!(reassembler.incomplete.len(), 0);
    }

    // --- Scheduler tests ---

    use tokio::io::AsyncReadExt;

    fn sender_pair() -> (AnySender, tokio::io::DuplexStream) {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (sender, _receiver) = crate::transport::generic_duplex(a);
        (AnySender::Generic(sender), b)
    }

    async fn read_wire_fragment(r: &mut tokio::io::DuplexStream) -> (Flags, Kind, u64, Vec<u8>) {
        let mut header_buf = [0u8; RawFragmentHeader::LEN];
        r.read_exact(&mut header_buf).await.unwrap();
        let (flags, kind, id, len) = RawFragmentHeader::decode(&header_buf).unwrap();
        let mut payload = vec![0u8; len];
        if len > 0 {
            r.read_exact(&mut payload).await.unwrap();
        }
        (flags, kind, id, payload)
    }

    #[test]
    fn fragment_shift_backs_off_on_short_write_and_decays_on_atomic_write() {
        let limits = Limits {
            max_fragment_size: 1024 + RawFragmentHeader::LEN,
            ..Limits::default()
        };
        let mut scheduler = new_scheduler(&limits);
        assert_eq!(scheduler.effective_fragment_size(), 1024);

        scheduler.record_write_atomicity(false);
        assert_eq!(scheduler.effective_fragment_size(), 512);
        scheduler.record_write_atomicity(false);
        assert_eq!(scheduler.effective_fragment_size(), 256);

        scheduler.record_write_atomicity(true);
        assert_eq!(scheduler.effective_fragment_size(), 512);
        scheduler.record_write_atomicity(true);
        assert_eq!(scheduler.effective_fragment_size(), 1024);

        // Never decays past the negotiated maximum.
        scheduler.record_write_atomicity(true);
        assert_eq!(scheduler.effective_fragment_size(), 1024);
    }

    #[test]
    fn fragment_shift_is_capped_and_size_never_reaches_zero() {
        let limits = Limits {
            max_fragment_size: 1024 + RawFragmentHeader::LEN,
            ..Limits::default()
        };
        let mut scheduler = new_scheduler(&limits);
        for _ in 0..20 {
            scheduler.record_write_atomicity(false);
        }
        assert_eq!(scheduler.fragment_shift, MAX_FRAGMENT_SHIFT);
        assert!(scheduler.effective_fragment_size() >= 1);
    }

    #[tokio::test]
    async fn scheduler_round_robins_between_active_messages() {
        let limits = Limits {
            max_fragment_size: 4 + RawFragmentHeader::LEN,
            ..Limits::default()
        };
        let mut scheduler = new_scheduler(&limits);
        scheduler.admit_message(
            Kind::Request,
            1,
            Bytes::from_static(b"AAAAAAAA"),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
        scheduler.admit_message(
            Kind::Request,
            2,
            Bytes::from_static(b"BBBBBBBB"),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
        let (mut sender, mut reader) = sender_pair();

        scheduler.advance(&mut sender).await.unwrap();
        let (_, _, id, _) = read_wire_fragment(&mut reader).await;
        assert_eq!(id, 1);

        scheduler.advance(&mut sender).await.unwrap();
        let (_, _, id, _) = read_wire_fragment(&mut reader).await;
        assert_eq!(
            id, 2,
            "second turn should serve the other message, not repeat id 1"
        );
    }

    #[tokio::test]
    async fn scheduler_prioritizes_ack() {
        let mut scheduler = new_scheduler(&Limits::default());
        scheduler.admit_message(
            Kind::Request,
            1,
            Bytes::from_static(b"request"),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
        scheduler.admit_empty(Kind::Ack, 2);
        let (mut sender, mut reader) = sender_pair();

        scheduler.advance(&mut sender).await.unwrap();
        let (flags, kind, id, payload) = read_wire_fragment(&mut reader).await;
        assert_eq!(kind, Kind::Ack);
        assert_eq!(id, 2);
        assert!(flags.contains(Flags::FIRST) && flags.contains(Flags::LAST));
        assert!(payload.is_empty());
    }

    #[tokio::test]
    async fn scheduler_single_fragment_message_bypasses_concurrency_gate() {
        let limits = Limits {
            max_fragment_size: 4 + RawFragmentHeader::LEN,
            max_concurrent_calls: 1,
            ..Limits::default()
        };
        let mut scheduler = new_scheduler(&limits);
        // Occupies the only fragmented-concurrency slot.
        scheduler.admit_message(
            Kind::Request,
            1,
            Bytes::from_static(b"AAAAAAAA"),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
        // Fits in one fragment; must not be blocked by the slot above.
        scheduler.admit_message(
            Kind::Request,
            2,
            Bytes::from_static(b"hi"),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
        let (mut sender, mut reader) = sender_pair();

        scheduler.advance(&mut sender).await.unwrap();
        let (_, _, id, _) = read_wire_fragment(&mut reader).await;
        assert_eq!(id, 1);

        scheduler.advance(&mut sender).await.unwrap();
        let (flags, _, id, payload) = read_wire_fragment(&mut reader).await;
        assert_eq!(id, 2);
        assert!(flags.contains(Flags::FIRST) && flags.contains(Flags::LAST));
        assert_eq!(payload, b"hi");
    }

    #[test]
    fn scheduler_defers_multi_fragment_message_when_active_fragmented_is_full() {
        let limits = Limits {
            max_fragment_size: 4 + RawFragmentHeader::LEN,
            max_concurrent_calls: 1,
            ..Limits::default()
        };
        let mut scheduler = new_scheduler(&limits);
        scheduler.admit_message(
            Kind::Request,
            1,
            Bytes::from_static(b"AAAAAAAA"),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
        scheduler.admit_message(
            Kind::Request,
            2,
            Bytes::from_static(b"BBBBBBBB"),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
        assert_eq!(scheduler.active.len(), 1);
        assert_eq!(scheduler.waiting.len(), 1);
        assert_eq!(scheduler.active_fragmented, 1);
    }

    #[tokio::test]
    async fn scheduler_wire_fragment_never_exceeds_max_fragment_size() {
        let limits = Limits {
            max_fragment_size: 20,
            ..Limits::default()
        };
        let mut scheduler = new_scheduler(&limits);
        scheduler.admit_message(
            Kind::Request,
            1,
            Bytes::from_static(b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
        let (mut sender, mut reader) = sender_pair();
        loop {
            scheduler.advance(&mut sender).await.unwrap();
            let (flags, _, _, payload) = read_wire_fragment(&mut reader).await;
            assert!(RawFragmentHeader::LEN + payload.len() <= limits.max_fragment_size);
            if flags.contains(Flags::LAST) {
                break;
            }
        }
    }

    #[tokio::test]
    async fn scheduler_promotes_waiting_message_when_a_slot_frees() {
        let limits = Limits {
            max_fragment_size: 4 + RawFragmentHeader::LEN,
            max_concurrent_calls: 1,
            ..Limits::default()
        };
        let mut scheduler = new_scheduler(&limits);
        scheduler.admit_message(
            Kind::Request,
            1,
            Bytes::from_static(b"AAAAAAAA"),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
        scheduler.admit_message(
            Kind::Request,
            2,
            Bytes::from_static(b"BBBBBBBB"),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
        assert_eq!(scheduler.waiting.len(), 1);
        let (mut sender, mut reader) = sender_pair();

        scheduler.advance(&mut sender).await.unwrap();
        let (flags, _, id, _) = read_wire_fragment(&mut reader).await;
        assert_eq!(id, 1);
        assert!(flags.contains(Flags::FIRST) && !flags.contains(Flags::LAST));
        assert_eq!(scheduler.waiting.len(), 1);

        scheduler.advance(&mut sender).await.unwrap();
        let (flags, _, id, _) = read_wire_fragment(&mut reader).await;
        assert_eq!(id, 1);
        assert!(flags.contains(Flags::LAST));
        assert_eq!(scheduler.waiting.len(), 0);
        assert_eq!(scheduler.active_fragmented, 1);
    }

    #[test]
    fn scheduler_try_cancel_active_reports_not_active_after_terminal_sent() {
        let mut scheduler = new_scheduler(&Limits::default());
        scheduler.admit_message(
            Kind::Request,
            1,
            Bytes::from_static(b"hi"),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
        scheduler.active.pop_front();
        assert!(matches!(
            scheduler.try_cancel_active(1),
            AbortOutcome::NotActive
        ));
    }

    #[test]
    fn scheduler_try_cancel_active_reports_not_started_before_any_fragment_sent() {
        let mut scheduler = new_scheduler(&Limits::default());
        scheduler.admit_message(
            Kind::Request,
            1,
            Bytes::from_static(b"AAAAAAAA"),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
        match scheduler.try_cancel_active(1) {
            AbortOutcome::Discarded { started, .. } => assert!(!started),
            AbortOutcome::NotActive => panic!("expected Discarded"),
        }
    }

    #[test]
    fn scheduler_try_cancel_active_reports_started_after_first_fragment() {
        let limits = Limits {
            max_fragment_size: 4 + RawFragmentHeader::LEN,
            ..Limits::default()
        };
        let mut scheduler = new_scheduler(&limits);
        scheduler.admit_message(
            Kind::Request,
            1,
            Bytes::from_static(b"AAAAAAAA"),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
        if let Some(send) = scheduler.active.front_mut() {
            send.offset = 4;
            send.started = true;
        }
        match scheduler.try_cancel_active(1) {
            AbortOutcome::Discarded { started, .. } => assert!(started),
            AbortOutcome::NotActive => panic!("expected Discarded"),
        }
    }

    #[test]
    fn scheduler_try_cancel_active_discards_waiting_message_without_abort() {
        let limits = Limits {
            max_fragment_size: 4 + RawFragmentHeader::LEN,
            max_concurrent_calls: 1,
            ..Limits::default()
        };
        let mut scheduler = new_scheduler(&limits);
        scheduler.admit_message(
            Kind::Request,
            1,
            Bytes::from_static(b"AAAAAAAA"),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
        scheduler.admit_message(
            Kind::Request,
            2,
            Bytes::from_static(b"BBBBBBBB"),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
        assert_eq!(scheduler.waiting.len(), 1);
        match scheduler.try_cancel_active(2) {
            AbortOutcome::Discarded { started, .. } => assert!(!started),
            AbortOutcome::NotActive => panic!("expected Discarded"),
        }
        assert_eq!(scheduler.waiting.len(), 0);
        assert_eq!(scheduler.active_fragmented, 1);
    }

    // --- Trailer scheduling tests ---

    #[test]
    fn scheduler_trailer_forces_multi_fragment_even_with_small_payload() {
        let limits = Limits {
            max_fragment_size: 1024 + RawFragmentHeader::LEN,
            max_concurrent_calls: 1,
            ..Limits::default()
        };
        let mut scheduler = new_scheduler(&limits);
        // A trailer forces multi_fragment (and a terminal commit), occupying
        // the only concurrency slot even with a tiny postcard payload.
        scheduler.admit_message(
            Kind::Request,
            1,
            Bytes::from_static(b"hi"),
            Default::default(),
            Trailer::Stream(SendShared::new(
                Kind::Request,
                1,
                &Limits { ..limits },
                Arc::new(SessionWindow::new(usize::MAX)),
            )),
            Default::default(),
        );
        assert_eq!(scheduler.active.len(), 1);
        assert_eq!(scheduler.active_fragmented, 1);
        // A second, ordinary small message with no trailer must not be
        // starved by the trailer message occupying the only slot.
        scheduler.admit_message(
            Kind::Request,
            2,
            Bytes::from_static(b"hi"),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
        assert_eq!(scheduler.active.len(), 2);
        assert_eq!(scheduler.waiting.len(), 0);
    }

    /// Trailer fragments that arrive after a local discard are sunk without
    /// costing either credit window: they were already in flight when the
    /// `Discard` went out, and they are never held in memory.
    #[tokio::test]
    async fn fragments_arriving_after_a_discard_cost_no_credit() {
        let limits = Limits {
            trailer_session_window: 1,
            ..Limits::default()
        };
        let mut bytes = fragment_bytes(Flags::FIRST | Flags::TRAILER, 1, Kind::Request, b"");
        bytes.extend(fragment_bytes(Flags::NONE, 1, Kind::Request, b"a"));
        bytes.extend(fragment_bytes(Flags::NONE, 1, Kind::Request, b"b"));
        let mut frame = FakeRecvFrame::new(bytes);
        let mut reassembler = new_reassembler(limits);

        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Message(msg) = reassembler.accept(header, &mut frame).await.unwrap() else {
            panic!("expected the payload boundary to dispatch the message");
        };
        let shared = msg.trailer.expect("trailer stream");
        RecvShared::discard(&shared);

        // Both fragments would exceed a one-byte window if they counted.
        for _ in 0..2 {
            let header = read_fragment_header(&mut frame).await.unwrap();
            let Event::Trailer { len, .. } = reassembler.accept(header, &mut frame).await.unwrap()
            else {
                panic!("expected a trailer-data fragment, not a credit violation");
            };
            drain_trailer_bytes(&mut frame, len).await;
        }
    }

    #[tokio::test]
    async fn scheduler_discard_active_trailer_errors_writer_and_sends_ordinary_terminal_commit() {
        use tokio::io::AsyncWriteExt;

        let limits = Limits::default();
        let mut scheduler = new_scheduler(&limits);
        let shared = SendShared::new(
            Kind::Request,
            1,
            &Limits { ..limits },
            Arc::new(SessionWindow::new(usize::MAX)),
        );
        scheduler.admit_message(
            Kind::Request,
            1,
            Bytes::from_static(b"hi"),
            Default::default(),
            Trailer::Stream(shared.clone()),
            Default::default(),
        );
        let mut trailer = crate::trailer::TrailerSend::new(shared, ());
        let (mut sender, mut reader) = sender_pair();

        // Postcard phase: FIRST plus TRAILER, since this is also the
        // payload's last fragment and a trailer is pending.
        scheduler.advance(&mut sender).await.unwrap();
        let (flags, _, id, payload) = read_wire_fragment(&mut reader).await;
        assert!(flags.contains(Flags::FIRST) && flags.contains(Flags::TRAILER));
        assert!(!flags.contains(Flags::LAST));
        assert_eq!(id, 1);
        assert_eq!(payload, b"hi");

        // One small trailer data fragment is staged without waiting for a
        // grant, then flushed by the scheduler.
        let writer = tokio::spawn(async move {
            trailer.write_all(b"data").await.unwrap();
            trailer
        });
        scheduler.advance(&mut sender).await.unwrap();
        let (flags, _, id, payload) = read_wire_fragment(&mut reader).await;
        assert_eq!(
            flags,
            Flags::NONE,
            "trailer data carries no flags of its own"
        );
        assert_eq!(id, 1);
        assert_eq!(payload, b"data");
        let mut trailer = writer.await.unwrap();

        // The peer no longer wants the rest of the trailer.
        scheduler.discard_active_trailer(1);

        // The writer observes a clean failure on its next write, not a
        // hang, since nothing will ever grant it another lease.
        let error = trailer.write_all(b"more").await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);

        // The next turn is an ordinary zero-length LAST terminal commit --
        // not an ABORT -- exactly as if the trailer had completed normally.
        scheduler.advance(&mut sender).await.unwrap();
        let (flags, _, id, payload) = read_wire_fragment(&mut reader).await;
        assert!(flags.contains(Flags::LAST));
        assert!(!flags.contains(Flags::ABORT));
        assert_eq!(id, 1);
        assert!(payload.is_empty());
    }

    // --- Negotiate tests ---

    /// A connected pair of full duplex (sender + receiver) endpoints, unlike
    /// `sender_pair` which only wires up one direction.
    fn duplex_endpoint_pair(buffer: usize) -> ((AnySender, AnyReceiver), (AnySender, AnyReceiver)) {
        let (a_to_b_write, a_to_b_read) = tokio::io::duplex(buffer);
        let (b_to_a_write, b_to_a_read) = tokio::io::duplex(buffer);
        let (a_sender, _unused) = crate::transport::generic_duplex(a_to_b_write);
        let (_unused, a_receiver) = crate::transport::generic_duplex(b_to_a_read);
        let (b_sender, _unused) = crate::transport::generic_duplex(b_to_a_write);
        let (_unused, b_receiver) = crate::transport::generic_duplex(a_to_b_read);
        (
            (
                AnySender::Generic(a_sender),
                AnyReceiver::Generic(a_receiver),
            ),
            (
                AnySender::Generic(b_sender),
                AnyReceiver::Generic(b_receiver),
            ),
        )
    }

    #[tokio::test]
    async fn negotiate_between_two_real_endpoints_selects_the_shared_version() {
        let ((mut a_sender, mut a_receiver), (mut b_sender, mut b_receiver)) =
            duplex_endpoint_pair(4096);
        let limits = Limits::default();
        let (a_result, b_result) = tokio::join!(
            negotiate(
                &mut a_sender,
                &mut a_receiver,
                &limits,
                ("test", &[1]),
                None
            ),
            negotiate(
                &mut b_sender,
                &mut b_receiver,
                &limits,
                ("test", &[1]),
                None
            ),
        );
        let a_result = a_result.unwrap();
        let b_result = b_result.unwrap();
        assert_eq!(a_result.version, PROTOCOL_VERSION);
        assert_eq!(b_result.version, PROTOCOL_VERSION);
        assert_eq!(a_result.app_protocol, ("test".to_string(), 1));
        assert_eq!(b_result.app_protocol, ("test".to_string(), 1));
    }

    #[tokio::test]
    async fn negotiate_aborts_and_fails_when_there_is_no_mutual_version() {
        let ((mut a_sender, mut a_receiver), (mut b_sender, mut b_receiver)) =
            duplex_endpoint_pair(4096);

        // `b` fakes a peer that only advertises a version `a` doesn't speak.
        let fake_peer = async move {
            let mut id = [0u8; 8];
            id[0] = PROTOCOL_VERSION.wrapping_add(1);
            let blob = postcard::to_stdvec(&0u8).unwrap();
            let payload = NegotiatePayload {
                version_blobs: vec![blob],
                app_protocol: ("test".to_string(), vec![1]),
            };
            let payload = postcard::to_stdvec(&payload).unwrap();
            write_negotiate_message(&mut b_sender, id, &payload)
                .await
                .unwrap();
            // First `a`'s own ordinary advertisement arrives (sent
            // concurrently with `a` reading ours); only after `a` processes
            // our list and finds no overlap does it send the ABORT failsafe.
            read_negotiate_message(&mut b_receiver).await.unwrap();
            let error = read_negotiate_message(&mut b_receiver).await.unwrap_err();
            assert!(matches!(error, Error::Protocol(_)));
        };

        let limits = Limits::default();
        let (a_result, ()) = tokio::join!(
            negotiate(
                &mut a_sender,
                &mut a_receiver,
                &limits,
                ("test", &[1]),
                None
            ),
            fake_peer,
        );
        assert!(matches!(a_result, Err(Error::Protocol(_))));
    }

    #[tokio::test]
    async fn negotiate_selects_max_overlapping_app_protocol_version() {
        let ((mut a_sender, mut a_receiver), (mut b_sender, mut b_receiver)) =
            duplex_endpoint_pair(4096);
        let limits = Limits::default();
        let (a_result, b_result) = tokio::join!(
            negotiate(
                &mut a_sender,
                &mut a_receiver,
                &limits,
                ("vfs", &[1, 2, 3]),
                None
            ),
            negotiate(
                &mut b_sender,
                &mut b_receiver,
                &limits,
                ("vfs", &[2, 3, 4]),
                None
            ),
        );
        let a_result = a_result.unwrap();
        let b_result = b_result.unwrap();
        assert_eq!(a_result.app_protocol, ("vfs".to_string(), 3));
        assert_eq!(b_result.app_protocol, ("vfs".to_string(), 3));
    }

    #[tokio::test]
    async fn negotiate_aborts_on_mismatched_app_protocol_name() {
        let ((mut a_sender, mut a_receiver), (mut b_sender, mut b_receiver)) =
            duplex_endpoint_pair(4096);
        let limits = Limits::default();
        let (a_result, b_result) = tokio::join!(
            negotiate(&mut a_sender, &mut a_receiver, &limits, ("vfs", &[1]), None),
            negotiate(
                &mut b_sender,
                &mut b_receiver,
                &limits,
                ("other", &[1]),
                None
            ),
        );
        let a_error = a_result.unwrap_err();
        let b_error = b_result.unwrap_err();
        assert!(
            matches!(a_error, Error::Protocol(ref msg) if msg.contains("mismatched application protocol"))
        );
        assert!(
            matches!(b_error, Error::Protocol(ref msg) if msg.contains("mismatched application protocol"))
        );
    }

    #[tokio::test]
    async fn negotiate_aborts_on_no_overlapping_app_protocol_version() {
        let ((mut a_sender, mut a_receiver), (mut b_sender, mut b_receiver)) =
            duplex_endpoint_pair(4096);
        let limits = Limits::default();
        let (a_result, b_result) = tokio::join!(
            negotiate(&mut a_sender, &mut a_receiver, &limits, ("vfs", &[1]), None),
            negotiate(&mut b_sender, &mut b_receiver, &limits, ("vfs", &[2]), None),
        );
        let a_error = a_result.unwrap_err();
        let b_error = b_result.unwrap_err();
        assert!(
            matches!(a_error, Error::Protocol(ref msg) if msg.contains("no mutually supported version of application protocol"))
        );
        assert!(
            matches!(b_error, Error::Protocol(ref msg) if msg.contains("no mutually supported version of application protocol"))
        );
    }

    const TEST_KEY: &[u8] = b"a-sufficiently-long-test-key";

    #[tokio::test]
    async fn negotiate_succeeds_when_both_ends_share_a_key() {
        let ((mut a_sender, mut a_receiver), (mut b_sender, mut b_receiver)) =
            duplex_endpoint_pair(4096);
        let limits = Limits::default();
        let key = crate::auth::AuthKey::new(TEST_KEY).unwrap();
        let (a_result, b_result) = tokio::join!(
            negotiate(
                &mut a_sender,
                &mut a_receiver,
                &limits,
                ("vfs", &[1]),
                Some(key.as_client())
            ),
            negotiate(
                &mut b_sender,
                &mut b_receiver,
                &limits,
                ("vfs", &[1]),
                Some(key.as_server())
            ),
        );
        assert_eq!(a_result.unwrap().app_protocol, ("vfs".to_string(), 1));
        assert_eq!(b_result.unwrap().app_protocol, ("vfs".to_string(), 1));
    }

    #[tokio::test]
    async fn negotiate_aborts_when_the_keys_differ() {
        let ((mut a_sender, mut a_receiver), (mut b_sender, mut b_receiver)) =
            duplex_endpoint_pair(4096);
        let limits = Limits::default();
        let a_key = crate::auth::AuthKey::new(TEST_KEY).unwrap();
        let b_key = crate::auth::AuthKey::new(b"an-entirely-different-key").unwrap();
        let (a_result, b_result) = tokio::join!(
            negotiate(
                &mut a_sender,
                &mut a_receiver,
                &limits,
                ("vfs", &[1]),
                Some(a_key.as_client())
            ),
            negotiate(
                &mut b_sender,
                &mut b_receiver,
                &limits,
                ("vfs", &[1]),
                Some(b_key.as_server())
            ),
        );
        let a_error = a_result.unwrap_err();
        let b_error = b_result.unwrap_err();
        assert!(matches!(a_error, Error::Auth(ref msg) if msg.contains("failed authentication")));
        assert!(matches!(b_error, Error::Auth(ref msg) if msg.contains("failed authentication")));
        // Neither side's digest may appear in a message that reaches a log.
        for error in [a_error, b_error] {
            let rendered = error.to_string();
            for digest in [a_key.as_client().advertise(), a_key.as_server().advertise()] {
                assert!(!rendered.contains(&hex(&digest)));
            }
        }
    }

    /// A peer that connects first, harvests the server's advertisement, and
    /// replays it as its own cannot authenticate: the two digests derive from
    /// different contexts, and neither yields the other.
    #[tokio::test]
    async fn negotiate_rejects_a_replayed_server_advertisement() {
        let ((mut a_sender, mut a_receiver), (mut b_sender, mut b_receiver)) =
            duplex_endpoint_pair(4096);
        let limits = Limits::default();
        let key = crate::auth::AuthKey::new(TEST_KEY).unwrap();
        let (a_result, b_result) = tokio::join!(
            negotiate(
                &mut a_sender,
                &mut a_receiver,
                &limits,
                ("vfs", &[1]),
                Some(key.as_server())
            ),
            negotiate(
                &mut b_sender,
                &mut b_receiver,
                &limits,
                ("vfs", &[1]),
                Some(key.as_server())
            ),
        );
        assert!(matches!(a_result.unwrap_err(), Error::Auth(_)));
        assert!(matches!(b_result.unwrap_err(), Error::Auth(_)));
    }

    #[tokio::test]
    async fn negotiate_aborts_when_only_one_end_is_keyed() {
        let key = crate::auth::AuthKey::new(TEST_KEY).unwrap();

        // Keyed client, unkeyed server: each side rejects independently, so a
        // configuration mistake cannot silently drop authentication.
        let ((mut a_sender, mut a_receiver), (mut b_sender, mut b_receiver)) =
            duplex_endpoint_pair(4096);
        let limits = Limits::default();
        let (a_result, b_result) = tokio::join!(
            negotiate(
                &mut a_sender,
                &mut a_receiver,
                &limits,
                ("vfs", &[1]),
                Some(key.as_client())
            ),
            negotiate(&mut b_sender, &mut b_receiver, &limits, ("vfs", &[1]), None),
        );
        assert!(
            matches!(a_result.unwrap_err(), Error::Auth(ref msg) if msg.contains("did not authenticate"))
        );
        assert!(
            matches!(b_result.unwrap_err(), Error::Auth(ref msg) if msg.contains("no key is configured"))
        );

        // ...and the reverse.
        let ((mut a_sender, mut a_receiver), (mut b_sender, mut b_receiver)) =
            duplex_endpoint_pair(4096);
        let (a_result, b_result) = tokio::join!(
            negotiate(&mut a_sender, &mut a_receiver, &limits, ("vfs", &[1]), None),
            negotiate(
                &mut b_sender,
                &mut b_receiver,
                &limits,
                ("vfs", &[1]),
                Some(key.as_server())
            ),
        );
        assert!(
            matches!(a_result.unwrap_err(), Error::Auth(ref msg) if msg.contains("no key is configured"))
        );
        assert!(
            matches!(b_result.unwrap_err(), Error::Auth(ref msg) if msg.contains("did not authenticate"))
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[tokio::test]
    async fn negotiate_message_spanning_multiple_fragments_reassembles() {
        let ((mut sender, _unused_receiver), (_unused_sender, mut receiver)) =
            duplex_endpoint_pair(1 << 20);
        let payload: Vec<u8> = (0..(NEGOTIATE_FRAGMENT_SIZE * 3 + 17))
            .map(|i| i as u8)
            .collect();
        let mut id = [0u8; 8];
        id[0] = 7;

        let (write_result, read_result) = tokio::join!(
            write_negotiate_message(&mut sender, id, &payload),
            read_negotiate_message(&mut receiver),
        );
        write_result.unwrap();
        let (got_id, got_payload) = read_result.unwrap();
        assert_eq!(got_id, id);
        assert_eq!(got_payload, payload);
    }

    #[tokio::test]
    async fn negotiate_message_exceeding_max_total_size_is_rejected() {
        let ((mut sender, _unused_receiver), (_unused_sender, mut receiver)) =
            duplex_endpoint_pair(1 << 21);
        let payload = vec![0u8; NEGOTIATE_MAX_PAYLOAD_SIZE + 1];
        let mut id = [0u8; 8];
        id[0] = 7;

        let (write_result, read_result) = tokio::join!(
            write_negotiate_message(&mut sender, id, &payload),
            read_negotiate_message(&mut receiver),
        );
        // The writer has no size limit of its own; only the reader enforces
        // the cap, so its write may or may not fail depending on how far
        // the reader got before erroring out and dropping the connection.
        let _ = write_result;
        let error = read_result.unwrap_err();
        assert!(
            matches!(error, Error::Protocol(ref msg) if msg.contains("exceeds the maximum tolerated total size"))
        );
    }

    #[tokio::test]
    async fn negotiate_write_and_read_do_not_deadlock_on_a_small_transport_buffer() {
        // Smaller than the multi-fragment payload below, so a naive
        // write-fully-then-read implementation on both sides would deadlock:
        // each side's write blocks on the other side draining it, which
        // never happens because the other side is also still blocked
        // writing.
        let ((mut a_sender, mut a_receiver), (mut b_sender, mut b_receiver)) =
            duplex_endpoint_pair(64);
        let payload = vec![0xABu8; NEGOTIATE_FRAGMENT_SIZE * 4];
        let mut id = [0u8; 8];
        id[0] = 3;

        let a_side = async {
            let (write_result, read_result) = tokio::join!(
                write_negotiate_message(&mut a_sender, id, &payload),
                read_negotiate_message(&mut a_receiver),
            );
            write_result.unwrap();
            read_result.unwrap().1
        };
        let b_side = async {
            let (write_result, read_result) = tokio::join!(
                write_negotiate_message(&mut b_sender, id, &payload),
                read_negotiate_message(&mut b_receiver),
            );
            write_result.unwrap();
            read_result.unwrap().1
        };

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(a_side, b_side)
        })
        .await
        .expect("negotiate write/read deadlocked on a small transport buffer");
        assert_eq!(result.0, payload);
        assert_eq!(result.1, payload);
    }

    /// A budget a test can watch, and the scheduler that spends it.
    fn quota_scheduler(limits: &Limits) -> (Scheduler, Arc<PayloadBudget>) {
        let budget = Arc::new(PayloadBudget::new(limits.max_outstanding_payload));
        (Scheduler::new(limits, budget.clone()), budget)
    }

    fn quota_limits(quota: usize) -> Limits {
        Limits {
            max_fragment_size: 4 + RawFragmentHeader::LEN,
            max_outstanding_payload: quota,
            ..Limits::default()
        }
    }

    fn admit(scheduler: &mut Scheduler, id: u64, payload: &'static [u8]) {
        scheduler.admit_message(
            Kind::Request,
            id,
            Bytes::from_static(payload),
            Default::default(),
            Trailer::None,
            Default::default(),
        );
    }

    /// The whole payload is charged before its first fragment goes out, so a
    /// message that does not fit does not start at all. Charging incrementally
    /// would let it start and then strand it half-sent with nothing able to
    /// finish it.
    #[test]
    fn a_message_that_does_not_fit_the_quota_waits_unstarted() {
        let limits = quota_limits(8);
        let (mut scheduler, budget) = quota_scheduler(&limits);

        admit(&mut scheduler, 1, b"AAAAAA");
        assert_eq!(budget.available(), 2, "charged its whole payload at once");
        assert_eq!(scheduler.active.len(), 1);

        admit(&mut scheduler, 2, b"BBBB");
        assert_eq!(scheduler.waiting.len(), 1, "does not fit in the remainder");
        assert_eq!(budget.available(), 2, "and so was charged nothing");
    }

    /// A quota-blocked send has to be visible to the poll that registers it on
    /// the pool, and invisible to the drain condition that would otherwise wait
    /// for credit no longer coming. Before the quota existed only a completing
    /// active send could promote, so a nonempty `waiting` implied a nonempty
    /// `active` and the two questions had one answer.
    #[tokio::test]
    async fn a_quota_blocked_send_is_pending_but_not_drainable_work() {
        let limits = quota_limits(4);
        let (mut scheduler, _budget) = quota_scheduler(&limits);
        admit(&mut scheduler, 1, b"AAAAAAAA");

        assert!(scheduler.waiting.len() == 1 && scheduler.active.is_empty());
        assert!(
            scheduler.has_pending(),
            "`ready` must be polled, or the credit that would start this send              arrives with nobody registered on the pool"
        );
        assert!(
            !scheduler.has_work(),
            "but it is not work a draining writer could ever finish: credit              comes through the receive half, which is gone by then"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), scheduler.ready())
                .await
                .is_err(),
            "an exhausted quota is a park, not a spin"
        );
    }

    /// Credit returned by the peer arrives on the pool from the reader, which
    /// can only wake the writer. Promotion therefore has to happen inside
    /// `ready`, or the wake-up finds nothing to do and parks again.
    #[tokio::test]
    async fn returned_quota_starts_a_waiting_send() {
        let limits = quota_limits(4);
        let (mut scheduler, budget) = quota_scheduler(&limits);
        admit(&mut scheduler, 1, b"AAAAAAAA");
        assert_eq!(scheduler.waiting.len(), 1);

        budget.credit(4);
        tokio::time::timeout(Duration::from_millis(50), scheduler.ready())
            .await
            .expect("credit should have made the send ready");
        assert_eq!(scheduler.active.len(), 1);
        assert_eq!(scheduler.waiting.len(), 0);
        assert_eq!(budget.available(), 0);
    }

    /// FIFO is the whole scheduling policy under quota constraint, and it is
    /// starvation-free precisely because a small message may not go around a
    /// large one that is merely short of credit.
    #[test]
    fn admission_out_of_the_waiting_queue_is_fifo() {
        let limits = quota_limits(8);
        let (mut scheduler, budget) = quota_scheduler(&limits);

        admit(&mut scheduler, 1, b"AAAAAAAA");
        admit(&mut scheduler, 2, b"BBBBBBBB");
        admit(&mut scheduler, 3, b"C");
        assert_eq!(scheduler.waiting.len(), 2, "the short one queues behind");

        budget.credit(8);
        scheduler.promote_waiting();
        assert_eq!(
            scheduler.active.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![1, 2],
            "the large message that was waiting first goes first"
        );
        assert_eq!(scheduler.waiting.len(), 1, "and 3 still waits its turn");
    }

    /// Cancellation splits the charge, because the two halves come back from
    /// different places: what never reached the wire is reclaimed locally, and
    /// what did is credited by the peer when it retires the aborted message.
    #[tokio::test]
    async fn cancelling_a_partly_sent_message_refunds_only_the_unsent_part() {
        let limits = quota_limits(8);
        let (mut scheduler, budget) = quota_scheduler(&limits);
        let (mut transport, mut wire) = sender_pair();

        admit(&mut scheduler, 1, b"AAAAAAAA");
        assert_eq!(budget.available(), 0);

        // One 4-byte fragment out of eight.
        scheduler.advance(&mut transport).await.unwrap();
        let (_, _, _, payload) = read_wire_fragment(&mut wire).await;
        assert_eq!(payload.len(), 4);

        assert!(matches!(
            scheduler.try_cancel_active(1),
            AbortOutcome::Discarded { started: true, .. }
        ));
        assert_eq!(
            budget.available(),
            4,
            "only the four bytes the peer never saw come back here"
        );
    }

    /// Nothing was charged for a send that never started, so cancelling it
    /// settles nothing and leaves the pool exactly as it was.
    #[test]
    fn cancelling_a_waiting_message_settles_nothing() {
        let limits = quota_limits(4);
        let (mut scheduler, budget) = quota_scheduler(&limits);
        admit(&mut scheduler, 1, b"AAAA");
        admit(&mut scheduler, 2, b"BBBB");
        assert_eq!(budget.available(), 0);

        assert!(matches!(
            scheduler.try_cancel_active(2),
            AbortOutcome::Discarded { started: false, .. }
        ));
        assert_eq!(budget.available(), 0, "message 1 still holds all of it");
    }

    /// Control fragments must never be gated by the quota. A peer parked on an
    /// exhausted pool is waiting for exactly these, and a handler blocked on an
    /// inbound trailer cannot complete — and therefore cannot release — until
    /// its credit gets out.
    #[tokio::test]
    async fn control_fragments_go_out_with_the_quota_exhausted() {
        let limits = quota_limits(4);
        let (mut scheduler, _budget) = quota_scheduler(&limits);
        let (mut transport, mut wire) = sender_pair();

        admit(&mut scheduler, 1, b"AAAA");
        admit(&mut scheduler, 2, b"BBBB");
        assert_eq!(scheduler.waiting.len(), 1);
        scheduler.admit_credit(9, 64);

        scheduler.advance(&mut transport).await.unwrap();
        let (_, kind, id, payload) = read_wire_fragment(&mut wire).await;
        assert_eq!(kind, Kind::Credit);
        assert_eq!(id, 9);
        assert_eq!(u32::from_le_bytes(payload.try_into().unwrap()), 64);
    }

    /// Payload credit names no message, so several releases collapse into one
    /// fragment rather than one apiece. Calls retire in bursts, and a fragment
    /// per retirement would be pure overhead.
    #[tokio::test]
    async fn payload_credit_coalesces_into_a_single_fragment() {
        let limits = Limits::default();
        let (mut scheduler, _budget) = quota_scheduler(&limits);
        let (mut transport, mut wire) = sender_pair();

        scheduler.admit_payload_credit(10);
        scheduler.admit_payload_credit(20);
        scheduler.admit_payload_credit(12);
        assert_eq!(scheduler.control.len(), 1);

        scheduler.advance(&mut transport).await.unwrap();
        let (flags, kind, id, payload) = read_wire_fragment(&mut wire).await;
        assert_eq!(kind, Kind::PayloadCredit);
        assert_eq!(flags, Flags::FIRST | Flags::LAST);
        assert_eq!(id, 0, "the id field is reserved and must be zero");
        assert_eq!(u32::from_le_bytes(payload.try_into().unwrap()), 42);
    }

    /// The aggregate backstop. Each message is legal on its own — the point of
    /// the limit is that their sum is not. A well-behaved peer parks rather
    /// than reaching this, so it is connection-fatal like the other framing
    /// violations.
    #[tokio::test]
    async fn rejects_messages_exceeding_the_session_payload_quota() {
        let limits = Limits {
            max_payload_size: 4,
            max_outstanding_payload: 6,
            ..Limits::default()
        };
        let mut reassembler = new_reassembler(limits);

        let mut frame = FakeRecvFrame::new(fast_path_bytes(1, Kind::Request, b"AAAA"));
        let header = read_fragment_header(&mut frame).await.unwrap();
        // Held, not dropped: the first message's charge is what leaves no room
        // for the second, and dropping it here would release it.
        let _first = reassembler.accept(header, &mut frame).await.unwrap();

        let mut frame = FakeRecvFrame::new(fast_path_bytes(2, Kind::Request, b"BBBB"));
        let header = read_fragment_header(&mut frame).await.unwrap();
        let Err(error) = reassembler.accept(header, &mut frame).await else {
            panic!("expected a protocol error");
        };
        assert!(
            format!("{error}").contains("session payload quota"),
            "unexpected error: {error}"
        );
    }

    /// Dropping the charge that travelled with a message is what returns its
    /// quota — to the pool and, through the sink, to the peer. Every way a call
    /// can end goes through this one drop.
    #[tokio::test]
    async fn retiring_a_message_returns_its_quota_to_the_peer() {
        struct Sink(std::sync::Mutex<Vec<u32>>);
        impl ControlSink for Arc<Sink> {
            fn credit(&self, _id: u64, _count: u32) {}
            fn payload_credit(&self, count: u32) {
                self.0.lock().unwrap().push(count);
            }
            fn discard(&self, _id: u64) {}
        }
        let sink = Arc::new(Sink(Default::default()));
        let limits = Limits {
            max_outstanding_payload: 16,
            ..Limits::default()
        };
        let mut reassembler = Reassembler::new(limits, Arc::new(sink.clone()));

        let mut frame = FakeRecvFrame::new(fast_path_bytes(1, Kind::Request, b"AAAA"));
        let header = read_fragment_header(&mut frame).await.unwrap();
        let Event::Message(message) = reassembler.accept(header, &mut frame).await.unwrap() else {
            panic!("expected a complete message");
        };
        assert_eq!(reassembler.payload_credit.available(), 12);
        assert!(sink.0.lock().unwrap().is_empty(), "not released yet");

        drop(message);
        assert_eq!(reassembler.payload_credit.available(), 16);
        assert_eq!(*sink.0.lock().unwrap(), vec![4]);
    }

    /// A message cancelled before its last fragment never reaches the
    /// application, so no charge is ever handed out to release it. The
    /// reassembler settles it instead — and credits the peer for what it did
    /// manage to send, which is buffered nowhere now.
    #[tokio::test]
    async fn aborting_a_partial_message_returns_its_quota() {
        let limits = Limits {
            max_fragment_size: 4 + RawFragmentHeader::LEN,
            max_outstanding_payload: 16,
            ..Limits::default()
        };
        let mut reassembler = new_reassembler(limits);

        let mut frame = FakeRecvFrame::new(fragment_bytes(Flags::FIRST, 1, Kind::Request, b"AAAA"));
        let header = read_fragment_header(&mut frame).await.unwrap();
        reassembler.accept(header, &mut frame).await.unwrap();
        assert_eq!(reassembler.payload_credit.available(), 12);

        let mut frame = FakeRecvFrame::new(fragment_bytes(Flags::ABORT, 1, Kind::Request, b""));
        let header = read_fragment_header(&mut frame).await.unwrap();
        reassembler.accept(header, &mut frame).await.unwrap();
        assert_eq!(reassembler.payload_credit.available(), 16);
    }

    /// The id field is reserved rather than merely unused, so a future
    /// revision can give it meaning without wondering what an old peer put
    /// there.
    #[tokio::test]
    async fn rejects_payload_credit_naming_a_message() {
        let mut reassembler = new_reassembler(Limits::default());
        let mut frame = FakeRecvFrame::new(fragment_bytes(
            Flags::FIRST | Flags::LAST,
            7,
            Kind::PayloadCredit,
            &1u32.to_le_bytes(),
        ));
        let header = read_fragment_header(&mut frame).await.unwrap();
        let Err(error) = reassembler.accept(header, &mut frame).await else {
            panic!("expected a protocol error");
        };
        assert!(
            format!("{error}").contains("must not name a message"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn rejects_empty_payload_credit() {
        let mut reassembler = new_reassembler(Limits::default());
        let mut frame = FakeRecvFrame::new(fragment_bytes(
            Flags::FIRST | Flags::LAST,
            0,
            Kind::PayloadCredit,
            &0u32.to_le_bytes(),
        ));
        let header = read_fragment_header(&mut frame).await.unwrap();
        let Err(error) = reassembler.accept(header, &mut frame).await else {
            panic!("expected a protocol error");
        };
        assert!(
            format!("{error}").contains("at least one byte"),
            "unexpected error: {error}"
        );
    }

    /// A per-message cap above the aggregate pool describes a message that
    /// could never be sent. Both ends can produce that combination — this end
    /// by configuration, the peer by advertising a small quota — so both are
    /// normalized rather than rejected.
    #[test]
    fn negotiation_keeps_the_quota_at_or_above_the_per_message_cap() {
        let generous = HandshakeV1::from_limits(
            &Limits {
                max_payload_size: 1024,
                max_outstanding_payload: 64 * 1024,
                ..Limits::default()
            },
            None,
        );
        let stingy = HandshakeV1::from_limits(
            &Limits {
                max_payload_size: 1024,
                max_outstanding_payload: 512,
                ..Limits::default()
            },
            None,
        );

        // A peer whose whole pool is smaller than our per-message cap drags
        // the cap down with it.
        let mut limits = Limits {
            max_payload_size: 1024,
            max_outstanding_payload: 64 * 1024,
            ..Limits::default()
        };
        stingy.clamp_limits(&mut limits);
        assert_eq!(limits.max_outstanding_payload, 512);
        assert_eq!(limits.max_payload_size, 512);

        // And a generous peer leaves the relationship alone.
        let mut limits = Limits {
            max_payload_size: 1024,
            max_outstanding_payload: 2048,
            ..Limits::default()
        };
        generous.clamp_limits(&mut limits);
        assert_eq!(limits.max_outstanding_payload, 2048);
        assert_eq!(limits.max_payload_size, 1024);
    }
}
