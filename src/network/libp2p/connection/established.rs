// Copyright (C) 2019-2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! State machine handling a single TCP or WebSocket libp2p connection.
//!
//! This state machine tries to negotiate and apply the noise and yamux protocols on top of the
//! connection.

use crate::network::leb128;

use super::{multistream_select, noise, yamux};

use core::{
    cmp, fmt, iter, mem,
    ops::{Add, Sub},
    time::Duration,
};

// TODO: needs a timeout for the handshake?

/// State machine of a fully-established connection.
pub struct Established<TNow, TRqUd, TNotifUd, TProtoList, TProto> {
    /// Encryption layer applied directly on top of the incoming data and outgoing data.
    /// In addition to the cipher state, also contains a buffer of data received from the socket,
    /// decoded but yet to be parsed.
    // TODO: move this decoded-data buffer here
    encryption: noise::Noise,
    /// State of the various substreams of the connection.
    /// Consists in a collection of substreams, each of which holding a [`Substream`] object.
    /// Also includes, for each substream, a collection of buffers whose data is to be written
    /// out.
    yamux: yamux::Yamux<Substream<TNow, TRqUd, TProtoList, TProto>>,

    /// Next substream timeout. When the current time is superior to this value, means that one of
    /// the substreams in `yamux` might have timed out.
    ///
    /// This value is not updated when a timeout is no longer necessary. As such, the value in
    /// this field might correspond to nothing (i.e. is now obsolete).
    next_timeout: Option<TNow>,

    /// See [`Config::in_request_protocols`].
    in_request_protocols: TProtoList,
    /// See [`Config::in_notifications_protocols`].
    in_notifications_protocols: TProtoList,
    /// See [`Config::ping_protocol`].
    ping_protocol: TProto,

    marker: core::marker::PhantomData<TNotifUd>, // TODO: remove
}

enum Substream<TNow, TRqUd, TProtoList, TProto> {
    /// Temporary transition state.
    Poisoned,

    /// Protocol negotiation in progress in an incoming substream.
    InboundNegotiating(
        multistream_select::InProgress<
            iter::Chain<iter::Chain<TProtoList, TProtoList>, iter::Once<TProto>>,
            TProto,
        >,
    ),

    /// Incoming substream has failed to negotiate a protocol. Waiting for a close from the remote.
    /// In order to save a round-trip time, the remote might assume that the protocol negotiation
    /// has succeeded. As such, it might send additional data on this substream that should be
    /// ignored.
    NegotiationFailed,

    /// Negotiating a protocol for a notifications protocol substream.
    NotificationsOutNegotiating {
        /// When the opening will time out in the absence of response.
        timeout: TNow,
        /// State of the protocol negotiation.
        negotiation: multistream_select::InProgress<TProtoList, TProto>,
        /// Bytes of the handshake to send after the substream is open.
        handshake: Vec<u8>,
    },

    /// A notifications protocol has been negotiated on a substream. Either a successful handshake
    /// or an abrupt closing is now expected.
    NotificationsOutHandshakeRecv {
        /// Buffer for the incoming handshake.
        handshake: leb128::Framed,
    },

    /// A notifications protocol has been negotiated, and the remote accepted it. Can now send
    /// notifications.
    NotificationsOut,

    /// A notifications protocol has been negotiated on an incoming substream. A handshake from
    /// the remote is expected.
    NotificationsInHandshake {
        /// Buffer for the incoming handshake.
        handshake: leb128::Framed,
        /// Protocol that was negotiated.
        protocol: TProto,
    },
    /// A handshake on a notifications protocol has been received. Now waiting for an action from
    /// the API user.
    NotificationsInWait,
    /// Negotiating a protocol for an outgoing request.
    RequestOutNegotiating {
        /// When the request will time out in the absence of response.
        timeout: TNow,
        /// State of the protocol negotiation.
        negotiation: multistream_select::InProgress<TProtoList, TProto>,
        /// Bytes of the request to send after the substream is open.
        request: Vec<u8>,
        /// Data passed by the user to [`Established::add_request`].
        user_data: TRqUd,
    },
    /// Outgoing request has been sent out or is queued for send out, and a response from the
    /// remote is now expected. Substream has been closed.
    RequestOut {
        /// When the request will time out in the absence of response.
        timeout: TNow,
        /// Data passed by the user to [`Established::add_request`].
        user_data: TRqUd,
        /// Buffer for the incoming response.
        response: leb128::Framed,
    },
    /// A request-response protocol has been negotiated on an inbound substream. A request is now
    /// expected.
    RequestInRecv {
        /// Buffer for the incoming request.
        request: leb128::Framed,
        /// Protocol that was negotiated.
        protocol: TProto,
    },
    RequestInSend,

    /// Inbound ping substream. Waiting for the ping payload to be received.
    PingIn(arrayvec::ArrayVec<[u8; 32]>),
}

impl<TNow, TRqUd, TNotifUd, TProtoList, TProto>
    Established<TNow, TRqUd, TNotifUd, TProtoList, TProto>
where
    TNow: Clone + Add<Duration, Output = TNow> + Sub<TNow, Output = Duration> + Ord,
    TProtoList: Iterator<Item = TProto> + Clone,
    TProto: AsRef<str> + Clone,
{
    /// Reads data coming from the socket from `incoming_data`, updates the internal state machine,
    /// and writes data destined to the socket to `outgoing_buffer`.
    ///
    /// `incoming_data` should be `None` if the remote has closed their writing side.
    ///
    /// The returned structure contains the number of bytes read and written from/to the two
    /// buffers. In order to avoid unnecessary memory allocations, only one [`Event`] is returned
    /// at a time. Consequently, this method returns as soon as an event is available, even if the
    /// buffers haven't finished being read. Call this method in a loop until these two values are
    /// both 0 and [`ReadWrite::event`] is `None`.
    ///
    /// If the remote isn't ready to accept new data, pass an empty slice as `outgoing_buffer`.
    ///
    /// The current time must be passed via the `now` parameter. This is used internally in order
    /// to keep track of ping times and timeouts. The returned structure optionally contains a
    /// `TNow` representing the moment after which this method should be called again.
    ///
    /// If an error is returned, the socket should be entirely shut down.
    // TODO: should take the in and out buffers as iterators, to allow for vectored reads/writes; tricky because an impl Iterator<Item = &mut [u8]> + Clone is impossible to build
    // TODO: in case of error, we're supposed to first send a yamux goaway frame
    pub fn read_write<'a>(
        mut self,
        now: TNow,
        mut incoming_buffer: Option<&[u8]>,
        mut outgoing_buffer: (&'a mut [u8], &'a mut [u8]),
    ) -> Result<ReadWrite<TNow, TRqUd, TNotifUd, TProtoList, TProto>, Error> {
        let mut total_read = 0;
        let mut total_written = 0;

        // First, check for timeouts.
        // Note that this might trigger timeouts for requests whose response is available in
        // `incoming_buffer`. This is intentional, as from the perspective of `read_write` the
        // response arrived after the timeout. It is the responsibility of the user to call
        // `read_write` in an appropriate way for this to not happen.
        if let Some(event) = self.update_now(now) {
            let wake_up_after = self.next_timeout.clone();
            return Ok(ReadWrite {
                connection: self,
                read_bytes: total_read,
                written_bytes: total_written,
                wake_up_after,
                event: Some(event),
            });
        }

        // Decoding the incoming data.
        loop {
            // Transfer data from `incoming_data` to the internal buffer in `self.encryption`.
            if let Some(incoming_data) = incoming_buffer.as_mut() {
                let num_read = self
                    .encryption
                    .inject_inbound_data(*incoming_data)
                    .map_err(Error::Noise)?;
                total_read += num_read;
                *incoming_data = &incoming_data[num_read..];
            }

            // TODO: handle incoming_data being None

            // Ask the Yamux state machine to decode the buffer present in `self.encryption`.
            let yamux_decode = self
                .yamux
                .incoming_data(self.encryption.decoded_inbound_data())
                .map_err(|err| Error::Yamux(err))?;
            self.yamux = yamux_decode.yamux;

            // TODO: it is possible that the yamux reading is blocked on writing

            // Analyze how Yamux has parsed the data.
            // This still contains references to the data in `self.encryption`.
            let (mut data, mut substream) = match yamux_decode.detail {
                None => {
                    if yamux_decode.bytes_read == 0 {
                        break;
                    }
                    self.encryption
                        .consume_inbound_data(yamux_decode.bytes_read);
                    continue;
                }

                Some(yamux::IncomingDataDetail::IncomingSubstream) => {
                    // Receive a request from the remote for a new incoming substream.
                    // These requests are automatically accepted.
                    // TODO: add a limit to the number of substreams
                    let nego =
                        multistream_select::InProgress::new(multistream_select::Config::Listener {
                            supported_protocols: self
                                .in_request_protocols
                                .clone()
                                .chain(self.in_notifications_protocols.clone())
                                .chain(iter::once(self.ping_protocol.clone())),
                        });
                    self.yamux
                        .accept_pending_substream(Substream::InboundNegotiating(nego));
                    self.encryption
                        .consume_inbound_data(yamux_decode.bytes_read);
                    continue;
                }

                Some(yamux::IncomingDataDetail::StreamReset {
                    substream_id,
                    user_data,
                }) => {
                    match user_data {
                        Substream::Poisoned => unreachable!(),
                        Substream::InboundNegotiating(_) => {}
                        Substream::NegotiationFailed => {}
                        Substream::RequestOutNegotiating { user_data, .. }
                        | Substream::RequestOut { user_data, .. } => {
                            let wake_up_after = self.next_timeout.clone();
                            return Ok(ReadWrite {
                                connection: self,
                                read_bytes: total_read,
                                written_bytes: total_written,
                                wake_up_after,
                                event: Some(Event::Response {
                                    id: SubstreamId(substream_id),
                                    user_data,
                                    response: Err(RequestError::SubstreamReset),
                                }),
                            });
                        }
                        Substream::RequestInRecv { .. } => {}
                        Substream::NotificationsInHandshake { .. } => {}
                        Substream::NotificationsInWait => {
                            // TODO: report to user
                            //todo!()
                        }
                        Substream::PingIn(_) => {}
                        _ => todo!("other substream kind"),
                    }
                    continue;
                }

                Some(yamux::IncomingDataDetail::DataFrame {
                    start_offset,
                    substream_id,
                }) => {
                    // Data belonging to a substream has been decoded.
                    let data = &self.encryption.decoded_inbound_data()
                        [start_offset..yamux_decode.bytes_read];
                    let substream = self.yamux.substream_by_id(substream_id).unwrap();
                    (data, substream)
                }
            };

            // This code is reached if data (`data` variable) has been received on a substream
            // (`substream` variable).
            while !data.is_empty() {
                // In order to solve borrowing-related issues, the block below temporarily
                // replaces the state of the substream with `Poisoned`, then later puts back a
                // proper state.
                match mem::replace(substream.user_data(), Substream::Poisoned) {
                    Substream::Poisoned => unreachable!(),
                    Substream::InboundNegotiating(nego) => {
                        match nego.read_write_vec(data) {
                            Ok((
                                multistream_select::Negotiation::InProgress(nego),
                                _read,
                                out_buffer,
                            )) => {
                                debug_assert_eq!(_read, data.len());
                                data = &data[_read..];
                                substream.write(out_buffer);
                                *substream.user_data() = Substream::InboundNegotiating(nego);
                            }
                            Ok((
                                multistream_select::Negotiation::Success(protocol),
                                num_read,
                                out_buffer,
                            )) => {
                                substream.write(out_buffer);
                                data = &data[num_read..];
                                if protocol.as_ref() == self.ping_protocol.as_ref() {
                                    *substream.user_data() = Substream::PingIn(Default::default());
                                } else {
                                    let is_request_response = self
                                        .in_request_protocols
                                        .clone()
                                        .any(|p| AsRef::as_ref(&p) == AsRef::as_ref(&protocol));
                                    if is_request_response {
                                        *substream.user_data() = Substream::RequestInRecv {
                                            protocol,
                                            request: leb128::Framed::new(10 * 1024 * 1024), // TODO: proper size
                                        };
                                    } else {
                                        *substream.user_data() =
                                            Substream::NotificationsInHandshake {
                                                protocol,
                                                handshake: leb128::Framed::new(10 * 1024 * 1024), // TODO: proper size
                                            };
                                    }
                                }
                            }
                            Ok((
                                multistream_select::Negotiation::NotAvailable,
                                num_read,
                                out_buffer,
                            )) => {
                                data = &data[num_read..];
                                substream.write(out_buffer);
                                substream.close();
                                *substream.user_data() = Substream::NegotiationFailed;
                            }
                            Err(_) => {
                                substream.reset();
                                break;
                            }
                        }
                    }
                    Substream::NegotiationFailed => {
                        // Substream is an inbound substream that has failed to negotiate a
                        // protocol. The substream is expected to close soon, but the remote might
                        // have been eagerly sending data (assuming that the negotiation would
                        // succeed), which should be silently discarded.
                        data = &data[data.len()..];
                        *substream.user_data() = Substream::NegotiationFailed;
                    }
                    Substream::NotificationsOutNegotiating {
                        negotiation,
                        timeout,
                        handshake,
                    } => {
                        match negotiation.read_write_vec(data) {
                            Ok((
                                multistream_select::Negotiation::InProgress(nego),
                                _read,
                                out_buffer,
                            )) => {
                                debug_assert_eq!(_read, data.len());
                                data = &data[_read..];
                                substream.write(out_buffer);
                                *substream.user_data() = Substream::NotificationsOutNegotiating {
                                    negotiation: nego,
                                    timeout,
                                    handshake,
                                };
                            }
                            Ok((
                                multistream_select::Negotiation::Success(_),
                                num_read,
                                out_buffer,
                            )) => {
                                substream.write(out_buffer);
                                data = &data[num_read..];
                                substream.write(leb128::encode_usize(handshake.len()).collect());
                                substream.write(handshake);
                                *substream.user_data() = Substream::NotificationsOutHandshakeRecv {
                                    handshake: leb128::Framed::new(10 * 1024 * 1024), // TODO: proper max size
                                };
                            }
                            _ => todo!(), // TODO:
                        }
                    }
                    Substream::NotificationsOutHandshakeRecv { mut handshake } => {
                        let num_read = handshake.inject_data(&data).unwrap(); // TODO: don't unwrap
                        data = &data[num_read..];
                        if let Some(handshake) = handshake.take_frame() {
                            let substream_id = substream.id();
                            todo!() // TODO:
                        }
                        *substream.user_data() =
                            Substream::NotificationsOutHandshakeRecv { handshake };
                    }
                    Substream::RequestOutNegotiating {
                        negotiation,
                        timeout,
                        request,
                        user_data,
                    } => {
                        match negotiation.read_write_vec(data) {
                            Ok((
                                multistream_select::Negotiation::InProgress(nego),
                                _read,
                                out_buffer,
                            )) => {
                                debug_assert_eq!(_read, data.len());
                                data = &data[_read..];
                                substream.write(out_buffer);
                                *substream.user_data() = Substream::RequestOutNegotiating {
                                    negotiation: nego,
                                    timeout,
                                    request,
                                    user_data,
                                };
                            }
                            Ok((
                                multistream_select::Negotiation::Success(_),
                                num_read,
                                out_buffer,
                            )) => {
                                substream.write(out_buffer);
                                data = &data[num_read..];
                                substream.write(leb128::encode_usize(request.len()).collect());
                                substream.write(request);
                                substream.close();
                                *substream.user_data() = Substream::RequestOut {
                                    timeout,
                                    user_data,
                                    response: leb128::Framed::new(10 * 1024 * 1024), // TODO: proper max size
                                };
                            }
                            Ok((multistream_select::Negotiation::NotAvailable, ..)) => {
                                let substream_id = substream.id();
                                substream.reset();
                                let wake_up_after = self.next_timeout.clone();
                                return Ok(ReadWrite {
                                    connection: self,
                                    read_bytes: total_read,
                                    written_bytes: total_written,
                                    wake_up_after,
                                    event: Some(Event::Response {
                                        id: SubstreamId(substream_id),
                                        user_data,
                                        response: Err(RequestError::ProtocolNotAvailable),
                                    }),
                                });
                            }
                            Err(err) => {
                                let substream_id = substream.id();
                                substream.reset();
                                let wake_up_after = self.next_timeout.clone();
                                return Ok(ReadWrite {
                                    connection: self,
                                    read_bytes: total_read,
                                    written_bytes: total_written,
                                    wake_up_after,
                                    event: Some(Event::Response {
                                        id: SubstreamId(substream_id),
                                        user_data,
                                        response: Err(RequestError::NegotiationError(err)),
                                    }),
                                });
                            }
                        }
                    }
                    Substream::RequestOut {
                        timeout,
                        user_data,
                        mut response,
                    } => {
                        let num_read = match response.inject_data(&data) {
                            Ok(n) => n,
                            Err(err) => {
                                let substream_id = substream.id();
                                substream.reset();
                                let wake_up_after = self.next_timeout.clone();
                                return Ok(ReadWrite {
                                    connection: self,
                                    read_bytes: total_read,
                                    written_bytes: total_written,
                                    wake_up_after,
                                    event: Some(Event::Response {
                                        id: SubstreamId(substream_id),
                                        user_data,
                                        response: Err(RequestError::ResponseLebError(err)),
                                    }),
                                });
                            }
                        };

                        data = &data[num_read..];
                        if let Some(response) = response.take_frame() {
                            let substream_id = substream.id();
                            // TODO: proper state transition
                            *substream.user_data() = Substream::NegotiationFailed;
                            let wake_up_after = self.next_timeout.clone();
                            return Ok(ReadWrite {
                                connection: self,
                                read_bytes: total_read,
                                written_bytes: total_written,
                                wake_up_after,
                                event: Some(Event::Response {
                                    id: SubstreamId(substream_id),
                                    user_data,
                                    response: Ok(response.into()),
                                }),
                            });
                        }
                        *substream.user_data() = Substream::RequestOut {
                            timeout,
                            user_data,
                            response,
                        };
                    }
                    Substream::RequestInRecv {
                        mut request,
                        protocol,
                    } => {
                        data = &data[data.len()..];
                        *substream.user_data() = Substream::RequestInRecv { request, protocol };
                        // TODO:
                        /*let num_read = request.inject_data(&data).unwrap(); // TODO: don't unwrap
                        if let Some(request) = request.take_frame() {
                            let substream_id = substream.id();
                            // TODO: state transition
                            let wake_up_after = self.next_timeout.clone();
                            return Ok(ReadWrite {
                                connection: self,
                                read_bytes: total_read,
                                written_bytes: total_written,
                                wake_up_after,
                                event: Some(Event::RequestIn {
                                    id: SubstreamId(substream_id),
                                    protocol,
                                    request: request.into(),
                                }),
                            });
                        }
                        todo!()*/
                    }
                    Substream::NotificationsInHandshake {
                        mut handshake,
                        protocol,
                    } => {
                        let num_read = handshake.inject_data(&data).unwrap(); // TODO: don't unwrap
                        data = &data[num_read..];
                        if let Some(handshake) = handshake.take_frame() {
                            let substream_id = substream.id();
                            *substream.user_data() = Substream::NotificationsInWait;
                            let wake_up_after = self.next_timeout.clone();
                            return Ok(ReadWrite {
                                connection: self,
                                read_bytes: total_read,
                                written_bytes: total_written,
                                wake_up_after,
                                event: Some(Event::NotificationsInOpen {
                                    id: SubstreamId(substream_id),
                                    protocol,
                                    handshake: handshake.into(),
                                }),
                            });
                        }
                        *substream.user_data() = Substream::NotificationsInHandshake {
                            handshake,
                            protocol,
                        };
                    }
                    Substream::NotificationsInWait => {
                        // TODO: what to do with data?
                        data = &data[data.len()..];
                        *substream.user_data() = Substream::NotificationsInWait;
                    }
                    Substream::PingIn(mut payload) => {
                        // Inbound ping substream.
                        // The ping protocol consists in sending 32 bytes of data, which the
                        // remote has to send back.
                        // The `payload` field contains these 32 bytes being received.
                        while !data.is_empty() {
                            debug_assert!(payload.len() < 32);
                            payload.push(data[0]);
                            data = &data[1..];

                            if payload.len() == 32 {
                                substream.write(payload.to_vec());
                                payload.clear();
                            }
                        }

                        *substream.user_data() = Substream::PingIn(payload);
                    }
                    _ => todo!("other substream kind"),
                }
            }

            // Now that the Yamux parsing has been processed, discard this data in
            // `self.encryption` and loop again, or stop looping if no more data is decodable.
            if yamux_decode.bytes_read == 0 {
                break;
            } else {
                self.encryption
                    .consume_inbound_data(yamux_decode.bytes_read);
            }
        }

        // The yamux state machine contains the data that needs to be written out.
        // Try to flush it.
        loop {
            let bytes_out = self
                .encryption
                .encrypt_size_conv(outgoing_buffer.0.len() + outgoing_buffer.1.len());
            if bytes_out == 0 {
                break;
            }

            let mut buffers = self.yamux.extract_out(bytes_out);
            let mut buffers = buffers.buffers().peekable();
            if buffers.peek().is_none() {
                break;
            }

            let (_read, written) = self
                .encryption
                .encrypt(buffers, (&mut outgoing_buffer.0, &mut outgoing_buffer.1));
            debug_assert!(_read <= bytes_out);
            total_written += written;
            let out_buf_0_len = outgoing_buffer.0.len();
            outgoing_buffer = (
                &mut outgoing_buffer.0[cmp::min(written, out_buf_0_len)..],
                &mut outgoing_buffer.1[written.saturating_sub(out_buf_0_len)..],
            );
            if outgoing_buffer.0.is_empty() {
                outgoing_buffer = (outgoing_buffer.1, outgoing_buffer.0);
            }
        }

        // Nothing more can be done.
        let wake_up_after = self.next_timeout.clone();
        Ok(ReadWrite {
            connection: self,
            read_bytes: total_read,
            written_bytes: total_written,
            wake_up_after,
            event: None,
        })
    }

    /// Updates the internal state machine, most notably `self.next_timeout`, with the passage of
    /// time.
    ///
    /// Optionally returns an event that happened as a result of the passage of time.
    fn update_now(&mut self, now: TNow) -> Option<Event<TRqUd, TNotifUd, TProto>> {
        if self.next_timeout.as_ref().map_or(true, |t| *t > now) {
            return None;
        }

        // Find which substream has timed out. This can be `None`, as the value in
        // `self.next_timeout` can be obsolete.
        let timed_out_substream = self
            .yamux
            .user_datas()
            .find(|(_, substream)| match &substream {
                Substream::RequestOutNegotiating { timeout, .. }
                | Substream::RequestOut { timeout, .. }
                    if *timeout <= now =>
                {
                    true
                }
                _ => false,
            })
            .map(|(id, _)| id);

        // Turn `timed_out_substream` into an `Event`.
        // The timed out substream (if any) is being reset'ted.
        let event = if let Some(timed_out_substream) = timed_out_substream {
            let substream = self
                .yamux
                .substream_by_id(timed_out_substream)
                .unwrap()
                .reset();

            Some(match substream {
                Substream::RequestOutNegotiating { user_data, .. }
                | Substream::RequestOut { user_data, .. } => Event::Response {
                    id: SubstreamId(timed_out_substream),
                    response: Err(RequestError::Timeout),
                    user_data,
                },
                _ => unreachable!(),
            })
        } else {
            None
        };

        // Update `next_timeout`. Note that some of the timeouts in `self.yamux` aren't
        // necessarily strictly superior to `now`. This is normal. As only one event can be
        // returned at a time, any further timeout will be handled the next time `update_now` is
        // called.
        self.next_timeout = self
            .yamux
            .user_datas()
            .filter_map(|(_, substream)| match &substream {
                Substream::RequestOutNegotiating { timeout, .. }
                | Substream::RequestOut { timeout, .. } => Some(timeout),
                _ => None,
            })
            .min()
            .cloned();

        event
    }

    /// Sends a request to the remote.
    ///
    /// This method only inserts the request into the connection object. Use
    /// [`Established::read_write`] in order to actually send out the request.
    ///
    /// Assuming that the remote is using the same implementation, an [`Event::RequestIn`] will
    /// be generated on its side.
    ///
    /// After the remote has sent back a response, an [`Event::Response`] event will be generated
    /// locally. The `user_data` parameter will be passed back.
    pub fn add_request(
        &mut self,
        now: TNow,
        protocol: TProto,
        request: Vec<u8>,
        user_data: TRqUd,
    ) -> SubstreamId {
        let mut negotiation =
            multistream_select::InProgress::new(multistream_select::Config::Dialer {
                requested_protocol: protocol,
            });

        let (new_state, _, out_buffer) = negotiation.read_write_vec(&[]).unwrap();
        match new_state {
            multistream_select::Negotiation::InProgress(n) => negotiation = n,
            _ => unreachable!(),
        }

        let timeout = now + Duration::from_secs(20); // TODO:

        if self.next_timeout.as_ref().map_or(true, |t| *t > timeout) {
            self.next_timeout = Some(timeout.clone());
        }

        let mut substream = self.yamux.open_substream(Substream::RequestOutNegotiating {
            timeout,
            negotiation,
            request,
            user_data,
        });

        substream.write(out_buffer);

        SubstreamId(substream.id())
    }

    /// Opens a outgoing substream with the given protocol, destined for a stream of
    /// notifications.
    ///
    /// The remote must first accept (or reject) the substream before notifications can be sent
    /// on it.
    ///
    /// This method only inserts the opening handshake into the connection object. Use
    /// [`Established::read_write`] in order to actually send out the request.
    ///
    /// Assuming that the remote is using the same implementation, an
    /// [`Event::NotificationsInOpen`] will be generated on its side.
    ///
    pub fn open_notifications_substream(
        &mut self,
        now: TNow,
        protocol: TProto,
        handshake: Vec<u8>,
    ) -> SubstreamId {
        let mut negotiation =
            multistream_select::InProgress::new(multistream_select::Config::Dialer {
                requested_protocol: protocol,
            });

        let (new_state, _, out_buffer) = negotiation.read_write_vec(&[]).unwrap();
        match new_state {
            multistream_select::Negotiation::InProgress(n) => negotiation = n,
            _ => unreachable!(),
        }

        let timeout = now + Duration::from_secs(20); // TODO:

        if self.next_timeout.as_ref().map_or(true, |t| *t > timeout) {
            self.next_timeout = Some(timeout.clone());
        }

        let mut substream = self
            .yamux
            .open_substream(Substream::NotificationsOutNegotiating {
                timeout,
                negotiation,
                handshake,
            });

        substream.write(out_buffer);

        SubstreamId(substream.id())
    }
}

impl<TNow, TRqUd, TNotifUd, TProtoList, TProto> fmt::Debug
    for Established<TNow, TRqUd, TNotifUd, TProtoList, TProto>
where
    TRqUd: fmt::Debug,
    TProto: AsRef<str>,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_map().entries(self.yamux.user_datas()).finish()
    }
}

impl<TNow, TRqUd, TProtoList, TProto> fmt::Debug for Substream<TNow, TRqUd, TProtoList, TProto>
where
    TRqUd: fmt::Debug,
    TProto: AsRef<str>,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Substream::Poisoned => f.debug_tuple("poisoned").finish(),
            Substream::NegotiationFailed => f.debug_tuple("incoming-negotiation-failed").finish(),
            Substream::InboundNegotiating(_) => f.debug_tuple("incoming-negotiating").finish(),
            Substream::NotificationsOutNegotiating { .. } => {
                todo!() // TODO:
            }
            Substream::NotificationsOutHandshakeRecv { .. } => {
                todo!() // TODO:
            }
            Substream::NotificationsOut => f.debug_tuple("notifications-out").finish(),
            Substream::NotificationsInHandshake { protocol, .. } => f
                .debug_tuple("notifications-in")
                .field(&AsRef::<str>::as_ref(protocol))
                .finish(),
            Substream::NotificationsInWait => {
                todo!() // TODO:
            }
            Substream::RequestOutNegotiating { user_data, .. }
            | Substream::RequestOut { user_data, .. } => {
                f.debug_tuple("request-out").field(&user_data).finish()
            }
            Substream::RequestInRecv { protocol, .. } => f
                .debug_tuple("request-in")
                .field(&AsRef::<str>::as_ref(protocol))
                .finish(),
            Substream::RequestInSend => {
                todo!() // TODO:
            }
            Substream::PingIn(_) => f.debug_tuple("ping-in").finish(),
        }
    }
}

/// Identifier of a request or a notifications substream.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubstreamId(yamux::SubstreamId);

/// Outcome of [`Established::read_write`].
#[must_use]
pub struct ReadWrite<TNow, TRqUd, TNotifUd, TProtoList, TProto> {
    /// Connection object yielded back.
    pub connection: Established<TNow, TRqUd, TNotifUd, TProtoList, TProto>,

    /// Number of bytes at the start of the incoming buffer that have been processed. These bytes
    /// should no longer be present the next time [`Established::read_write`] is called.
    pub read_bytes: usize,

    /// Number of bytes written to the outgoing buffer. These bytes should be sent out to the
    /// remote. The rest of the outgoing buffer is left untouched.
    pub written_bytes: usize,

    /// If `Some`, [`Established::read_write`] should be called again when the point in time
    /// reaches the value in the `Option`.
    pub wake_up_after: Option<TNow>,

    /// Event that happened on the connection.
    pub event: Option<Event<TRqUd, TNotifUd, TProto>>,
}

/// Event that happened on the connection. See [`ReadWrite::event`].
#[must_use]
#[derive(Debug)]
pub enum Event<TRqUd, TNotifUd, TProto> {
    /// No more outgoing data will be emitted. The local writing side of the connection should be
    /// closed.
    // TODO: remove?
    EndOfData,
    /// Received a request in the context of a request-response protocol.
    RequestIn {
        /// Identifier of the request. Needs to be provided back when answering the request.
        id: SubstreamId,
        /// Protocol the request was sent on.
        protocol: TProto,
        /// Bytes of the request. Its interpretation is out of scope of this module.
        request: Vec<u8>,
    },
    /// Received a response to a previously emitted request on a request-response protocol.
    Response {
        /// Bytes of the response. Its interpretation is out of scope of this module.
        response: Result<Vec<u8>, RequestError>,
        /// Identifier of the request. Value that was returned by [`Established::add_request`].
        id: SubstreamId,
        /// Value that was passed to [`Established::add_request`].
        user_data: TRqUd,
    },
    NotificationsInOpen {
        /// Identifier of the substream. Needs to be provided back when accept or rejecting the
        /// substream.
        id: SubstreamId,
        /// Protocol concerned by the substream.
        protocol: TProto,
        /// Handshake sent by the remote. Its interpretation is out of scope of this module.
        handshake: Vec<u8>,
    },
    /// Remote has accepted a substream opened with [`Established::open_notifications_substream`].
    ///
    /// It is now possible to send notifications on this substream.
    NotificationsOutAccept {
        /// Identifier of the substream. Value that was returned by
        /// [`Established::open_notifications_substream`].
        id: SubstreamId,
        /// Value that was passed to [`Established::open_notifications_substream`].
        user_data: TNotifUd,
        /// Handshake sent back by the remote. Its interpretation is out of scope of this module.
        remote_handshake: Vec<u8>,
    },
    /// Remote has rejected a substream opened with [`Established::open_notifications_substream`].
    NotificationsOutReject {
        /// Identifier of the substream. Value that was returned by
        /// [`Established::open_notifications_substream`].
        id: SubstreamId,
        /// Value that was passed to [`Established::open_notifications_substream`].
        user_data: TNotifUd,
    },
}

/// Error during a connection. The connection should be shut down.
#[derive(Debug, derive_more::Display)]
pub enum Error {
    /// Error in the noise cipher. Data has most likely been corrupted.
    Noise(noise::CipherError),
    /// Error in the yamux multiplexing protocol.
    Yamux(yamux::Error),
}

/// Error that can happen during a request in a request-response scheme.
#[derive(Debug, derive_more::Display)]
pub enum RequestError {
    /// Remote hasn't answered in time.
    Timeout,
    /// Remote doesn't support this protocol.
    ProtocolNotAvailable,
    /// Remote has decided to RST the substream. This most likely indicates that the remote has
    /// detected a protocol error.
    SubstreamReset,
    /// Error during protocol negotiation.
    NegotiationError(multistream_select::Error),
    /// Error while receiving the response.
    ResponseLebError(leb128::FramedError),
}

/// Successfully negotiated connection. Ready to be turned into a [`Established`].
pub struct ConnectionPrototype {
    encryption: noise::Noise,
}

impl ConnectionPrototype {
    /// Builds a new [`ConnectionPrototype`] of a connection using the Noise and Yamux protocols.
    pub(crate) fn from_noise_yamux(encryption: noise::Noise) -> Self {
        ConnectionPrototype { encryption }
    }

    /// Turns this prototype into an actual connection.
    pub fn into_connection<TNow, TRqUd, TNotifUd, TProtoList, TProto>(
        self,
        config: Config<TProtoList, TProto>,
    ) -> Established<TNow, TRqUd, TNotifUd, TProtoList, TProto> {
        // TODO: check conflicts between protocol names?

        let yamux = yamux::Yamux::new(yamux::Config {
            is_initiator: self.encryption.is_initiator(),
            capacity: 64, // TODO: ?
            randomness_seed: config.randomness_seed,
        });

        Established {
            encryption: self.encryption,
            yamux,
            next_timeout: None,
            in_request_protocols: config.in_request_protocols,
            in_notifications_protocols: config.in_notifications_protocols,
            ping_protocol: config.ping_protocol,
            marker: core::marker::PhantomData,
        }
    }
}

impl fmt::Debug for ConnectionPrototype {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ConnectionPrototype").finish()
    }
}

/// Configuration to turn a [`ConnectionPrototype`] into a [`Established`].
pub struct Config<TProtoList, TProto> {
    /// List of request-response protocols supported for incoming substreams.
    pub in_request_protocols: TProtoList,
    /// List of notifications protocols supported for incoming substreams.
    pub in_notifications_protocols: TProtoList,
    /// Name of the ping protocol on the network.
    pub ping_protocol: TProto,
    /// Seed used for the randomness specific to this connection.
    pub randomness_seed: (u64, u64),
}
