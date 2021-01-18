// Copyright (c) 2019 Parity Technologies Limited
//
// Permission is hereby granted, free of charge, to any
// person obtaining a copy of this software and associated
// documentation files (the "Software"), to deal in the
// Software without restriction, including without
// limitation the rights to use, copy, modify, merge,
// publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software
// is furnished to do so, subject to the following
// conditions:
//
// The above copyright notice and this permission notice
// shall be included in all copies or substantial portions
// of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
// ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
// TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
// PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
// SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
// CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
// IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use crate::transport::{TransportServerEvent, WsRequestId as RequestId, WsTransportServer};
use jsonrpsee_types::{
	jsonrpc::wrapped::{batches, Notification, Params},
	jsonrpc::{self, JsonValue},
};

use alloc::{borrow::ToOwned as _, string::String, vec, vec::Vec};
use core::convert::TryFrom;
use core::{fmt, hash::Hash, num::NonZeroUsize};
use hashbrown::{hash_map::Entry, HashMap};

/// Wraps around a "raw server" and adds capabilities.
///
/// See the module-level documentation for more information.
pub struct RawServer {
	/// Internal "raw" server.
	raw: WsTransportServer,

	/// List of requests that are in the progress of being answered. Each batch is associated with
	/// the raw request ID, or with `None` if this raw request has been closed.
	///
	/// See the documentation of [`BatchesState`][batches::BatchesState] for more information.
	batches: batches::BatchesState<Option<RequestId>>,

	/// List of active subscriptions.
	/// The identifier is chosen randomly and uniformy distributed. It is never decided by the
	/// client. There is therefore no risk of hash collision attack.
	subscriptions: HashMap<[u8; 32], SubscriptionState<RequestId>, fnv::FnvBuildHasher>,

	/// For each raw request ID (i.e. client connection), the number of active subscriptions
	/// that are using it.
	///
	/// If this reaches 0, we can tell the raw server to close the request.
	///
	/// Because we don't have any information about `I`, we have to use a collision-resistant
	/// hashing algorithm. This incurs a performance cost that is theoretically avoidable (if `I`
	/// is always local), but that should be negligible in practice.
	num_subscriptions: HashMap<RequestId, NonZeroUsize>,
}

/// Identifier of a request within a `RawServer`.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct RawServerRequestId {
	inner: batches::BatchesElemId,
}

/// Identifier of a subscription within a [`RawServer`](crate::server::RawServer).
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct RawServerSubscriptionId([u8; 32]);

/// Event generated by a [`RawServer`](crate::server::RawServer).
///
/// > **Note**: Holds a borrow of the `RawServer`. Therefore, must be dropped before the `RawServer` can
/// >           be dropped.
#[derive(Debug)]
pub enum RawServerEvent<'a> {
	/// Request is a notification.
	Notification(Notification),

	/// Request is a method call.
	Request(RawServerRequest<'a>),

	/// Subscriptions are now ready.
	SubscriptionsReady(SubscriptionsReadyIter),

	/// Subscriptions have been closed because the client closed the connection.
	SubscriptionsClosed(SubscriptionsClosedIter),
}

/// Request received by a [`RawServer`](crate::raw::RawServer).
pub struct RawServerRequest<'a> {
	/// Reference to the request within `self.batches`.
	inner: batches::BatchesElem<'a, Option<RequestId>>,

	/// Reference to the corresponding field in `RawServer`.
	raw: &'a mut WsTransportServer,

	/// Pending subscriptions.
	subscriptions: &'a mut HashMap<[u8; 32], SubscriptionState<RequestId>, fnv::FnvBuildHasher>,

	/// Reference to the corresponding field in `RawServer`.
	num_subscriptions: &'a mut HashMap<RequestId, NonZeroUsize>,
}

/// Active subscription of a client towards a server.
///
/// > **Note**: Holds a borrow of the `RawServer`. Therefore, must be dropped before the `RawServer` can
/// >           be dropped.
pub struct ServerSubscription<'a> {
	server: &'a mut RawServer,
	id: [u8; 32],
}

/// Error that can happen when calling `into_subscription`.
#[derive(Debug)]
pub enum IntoSubscriptionErr {
	/// Underlying server doesn't support subscriptions.
	NotSupported,
	/// Request has already been closed by the client.
	Closed,
}

/// Iterator for the list of subscriptions that are now ready.
#[derive(Debug)]
pub struct SubscriptionsReadyIter(vec::IntoIter<RawServerSubscriptionId>);

/// Iterator for the list of subscriptions that have been closed.
#[derive(Debug)]
pub struct SubscriptionsClosedIter(vec::IntoIter<RawServerSubscriptionId>);

/// Internal structure. Information about a subscription.
#[derive(Debug)]
struct SubscriptionState<I> {
	/// Identifier of the connection in the raw server.
	raw_id: I,
	/// Method that triggered the subscription. Must be sent to the client at each notification.
	method: String,
	/// If true, the subscription shouldn't accept any notification push because the confirmation
	/// hasn't been sent to the client yet. Once this has switched to `false`, it can never be
	/// switched to `true` ever again.
	pending: bool,
}

impl RawServer {
	/// Starts a [`RawServer`](crate::raw::RawServer) using the given raw server internally.
	pub fn new(raw: WsTransportServer) -> RawServer {
		RawServer {
			raw,
			batches: batches::BatchesState::new(),
			subscriptions: HashMap::with_capacity_and_hasher(8, Default::default()),
			num_subscriptions: HashMap::with_capacity_and_hasher(8, Default::default()),
		}
	}
}

impl RawServer {
	/// Returns a `Future` resolving to the next event that this server generates.
	pub async fn next_event<'a>(&'a mut self) -> RawServerEvent<'a> {
		let request_id = loop {
			match self.batches.next_event() {
				None => {}
				Some(batches::BatchesEvent::Notification { notification, .. }) => {
					return RawServerEvent::Notification(notification)
				}
				Some(batches::BatchesEvent::Request(inner)) => {
					break RawServerRequestId { inner: inner.id() };
				}
				Some(batches::BatchesEvent::ReadyToSend { response, user_param: Some(raw_request_id) }) => {
					// If we have any active subscription, we only use `send` to not close the
					// client request.
					if self.num_subscriptions.contains_key(&raw_request_id) {
						debug_assert!(self.raw.supports_resuming(&raw_request_id).unwrap_or(false));
						let _ = self.raw.send(&raw_request_id, &response).await;
						// TODO: that's O(n)
						let mut ready = Vec::new(); // TODO: with_capacity
						for (sub_id, sub) in self.subscriptions.iter_mut() {
							if sub.raw_id == raw_request_id {
								ready.push(RawServerSubscriptionId(sub_id.clone()));
								sub.pending = false;
							}
						}
						debug_assert!(!ready.is_empty()); // TODO: assert that capacity == len
						return RawServerEvent::SubscriptionsReady(SubscriptionsReadyIter(ready.into_iter()));
					} else {
						let _ = self.raw.finish(&raw_request_id, Some(&response)).await;
					}
					continue;
				}
				Some(batches::BatchesEvent::ReadyToSend { response: _, user_param: None }) => {
					// This situation happens if the connection has been closed by the client.
					// Clients who close their connection.
					continue;
				}
			};

			match self.raw.next_request().await {
				TransportServerEvent::Request { id, request } => self.batches.inject(request, Some(id)),
				TransportServerEvent::Closed(raw_id) => {
					// The client has a closed their connection. We eliminate all traces of the
					// raw request ID from our state.
					// TODO: this has an O(n) complexity; make sure that this is not attackable
					for ud in self.batches.batches() {
						if ud.as_ref() == Some(&raw_id) {
							*ud = None;
						}
					}

					// Additionally, active subscriptions that were using this connection are
					// closed.
					if let Some(_) = self.num_subscriptions.remove(&raw_id) {
						let ids = self
							.subscriptions
							.iter()
							.filter(|(_, v)| v.raw_id == raw_id)
							.map(|(k, _)| RawServerSubscriptionId(*k))
							.collect::<Vec<_>>();
						for id in &ids {
							let _ = self.subscriptions.remove(&id.0);
						}
						return RawServerEvent::SubscriptionsClosed(SubscriptionsClosedIter(ids.into_iter()));
					}
				}
			};
		};

		RawServerEvent::Request(self.request_by_id(&request_id).unwrap())
	}

	/// Returns a request previously returned by [`next_event`](crate::raw::RawServer::next_event)
	/// by its id.
	///
	/// Note that previous notifications don't have an ID and can't be accessed with this method.
	///
	/// Returns `None` if the request ID is invalid or if the request has already been answered in
	/// the past.
	pub fn request_by_id<'a>(&'a mut self, id: &RawServerRequestId) -> Option<RawServerRequest<'a>> {
		Some(RawServerRequest {
			inner: self.batches.request_by_id(id.inner)?,
			raw: &mut self.raw,
			subscriptions: &mut self.subscriptions,
			num_subscriptions: &mut self.num_subscriptions,
		})
	}

	/// Returns a subscription previously returned by
	/// [`into_subscription`](crate::raw::server::RawServerRequest::into_subscription).
	pub fn subscription_by_id(&mut self, id: RawServerSubscriptionId) -> Option<ServerSubscription> {
		if self.subscriptions.contains_key(&id.0) {
			Some(ServerSubscription { server: self, id: id.0 })
		} else {
			None
		}
	}
}

impl From<WsTransportServer> for RawServer {
	fn from(inner: WsTransportServer) -> Self {
		RawServer::new(inner)
	}
}

impl<'a> RawServerRequest<'a> {
	/// Returns the id of the request.
	///
	/// If this request object is dropped, you can retreive it again later by calling
	/// [`request_by_id`](crate::raw::RawServer::request_by_id).
	pub fn id(&self) -> RawServerRequestId {
		RawServerRequestId { inner: self.inner.id() }
	}

	/// Returns the id that the client sent out.
	// TODO: can return None, which is wrong
	pub fn request_id(&self) -> &jsonrpc::Id {
		self.inner.request_id()
	}

	/// Returns the method of this request.
	pub fn method(&self) -> &str {
		self.inner.method()
	}

	/// Returns the parameters of the request, as a `jsonrpc::Params`.
	pub fn params(&self) -> Params {
		self.inner.params()
	}
}

impl<'a> RawServerRequest<'a> {
	/// Send back a response.
	///
	/// If this request is part of a batch:
	///
	/// - If all requests of the batch have been responded to, then the response is actively
	///   sent out.
	/// - Otherwise, this response is buffered.
	///
	/// > **Note**: This method is implemented in a way that doesn't wait for long to send the
	/// >           response. While calling this method will block your entire server, it
	/// >           should only block it for a short amount of time. See also [the equivalent
	/// >           method](crate::transport::TransportServer::finish) on the
	/// >           [`TransportServer`](crate::transport::TransportServer) trait.
	///
	pub fn respond(self, response: Result<JsonValue, jsonrpc::Error>) {
		self.inner.set_response(response);
		//unimplemented!();
		// TODO: actually send out response?
	}

	/// Sends back a response similar to `respond`, then returns a [`RawServerSubscriptionId`] object
	/// that allows you to push more data on the corresponding connection.
	///
	/// The [`RawServerSubscriptionId`] corresponds to the identifier that has been sent back to the
	/// client. If the client refers to this subscription id, you can turn it into a
	/// [`RawServerSubscriptionId`] using
	/// [`from_wire_message`](RawServerSubscriptionId::from_wire_message).
	///
	/// After the request has been turned into a subscription, the subscription might be in
	/// "pending mode". Pushing notifications on that subscription will return an error. This
	/// mechanism is necessary because the subscription request might be part of a batch, and all
	/// the requests of that batch have to be processed before informing the client of the start
	/// of the subscription.
	///
	/// Returns an error and doesn't do anything if the underlying server doesn't support
	/// subscriptions, or if the connection has already been closed by the client.
	///
	/// > **Note**: Because of borrowing issues, we return a [`RawServerSubscriptionId`] rather than
	/// >           a [`ServerSubscription`]. You will have to call
	/// >           [`subscription_by_id`](RawServer::subscription_by_id) in order to manipulate the
	/// >           subscription.
	// TODO: solve the note
	pub fn into_subscription(mut self) -> Result<RawServerSubscriptionId, IntoSubscriptionErr> {
		let raw_request_id = match self.inner.user_param().clone() {
			Some(id) => id,
			None => return Err(IntoSubscriptionErr::Closed),
		};

		if !self.raw.supports_resuming(&raw_request_id).unwrap_or(false) {
			return Err(IntoSubscriptionErr::NotSupported);
		}

		loop {
			let new_subscr_id: [u8; 32] = rand::random();

			match self.subscriptions.entry(new_subscr_id) {
				Entry::Vacant(e) => e.insert(SubscriptionState {
					raw_id: raw_request_id.clone(),
					method: self.inner.method().to_owned(),
					pending: true,
				}),
				// Continue looping if we accidentally chose an existing ID.
				Entry::Occupied(_) => continue,
			};

			self.num_subscriptions
				.entry(raw_request_id)
				.and_modify(|e| {
					*e = NonZeroUsize::new(e.get() + 1).expect("we add 1 to an existing non-zero value; qed");
				})
				.or_insert_with(|| NonZeroUsize::new(1).expect("1 != 0"));

			let subscr_id_string = bs58::encode(&new_subscr_id).into_string();
			self.inner.set_response(Ok(subscr_id_string.into()));
			break Ok(RawServerSubscriptionId(new_subscr_id));
		}
	}
}

impl<'a> fmt::Debug for RawServerRequest<'a> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("RawServerRequest")
			.field("request_id", &self.request_id())
			.field("method", &self.method())
			.field("params", &self.params())
			.finish()
	}
}

impl RawServerSubscriptionId {
	/// When the client sends a unsubscribe message containing a subscription ID, this function can
	/// be used to parse it into a [`RawServerSubscriptionId`].
	pub fn from_wire_message(params: &JsonValue) -> Result<Self, ()> {
		let string = match params {
			JsonValue::String(s) => s,
			_ => return Err(()),
		};

		let decoded = bs58::decode(&string).into_vec().map_err(|_| ())?;
		if decoded.len() > 32 {
			return Err(());
		}

		let mut out = [0; 32];
		out[(32 - decoded.len())..].copy_from_slice(&decoded);
		// TODO: write a test to check that encoding/decoding match
		Ok(RawServerSubscriptionId(out))
	}
}

// Try to parse a subscription ID from `Params` where we try both index 0 of an array or `subscription`
// in a `Map`.
impl<'a> TryFrom<Params<'a>> for RawServerSubscriptionId {
	type Error = ();

	fn try_from(params: Params) -> Result<Self, Self::Error> {
		if let Ok(other_id) = params.get(0) {
			Self::from_wire_message(&other_id)
		} else if let Ok(other_id) = params.get("subscription") {
			Self::from_wire_message(&other_id)
		} else {
			Err(())
		}
	}
}

impl<'a> ServerSubscription<'a> {
	/// Returns the id of the subscription.
	///
	/// If this subscription object is dropped, you can retreive it again later by calling
	/// [`subscription_by_id`](crate::raw::RawServer::subscription_by_id).
	pub fn id(&self) -> RawServerSubscriptionId {
		RawServerSubscriptionId(self.id)
	}

	/// Pushes a notification.
	///
	// TODO: refactor to progate the error.
	pub async fn push(self, message: impl Into<JsonValue>) {
		let subscription_state = self.server.subscriptions.get(&self.id).unwrap();
		if subscription_state.pending {
			return; // TODO: notify user with error
		}

		let output = jsonrpc::SubscriptionNotif {
			jsonrpc: jsonrpc::Version::V2,
			method: subscription_state.method.clone(),
			params: jsonrpc::SubscriptionNotifParams {
				subscription: jsonrpc::SubscriptionId::Str(bs58::encode(&self.id).into_string()),
				result: message.into(),
			},
		};
		let response = jsonrpc::Response::Notif(output);

		let _ = self.server.raw.send(&subscription_state.raw_id, &response).await; // TODO: error handling?
	}

	/// Destroys the subscription object.
	///
	/// This does not send any message back to the client. Instead, this function is supposed to
	/// be used in reaction to the client requesting to be unsubscribed.
	///
	/// If this was the last active subscription, also closes the connection ("raw request") with
	/// the client.
	pub async fn close(self) {
		let subscription_state = self.server.subscriptions.remove(&self.id).unwrap();

		// Check if we're the last subscription on this connection.
		// Remove entry from `num_subscriptions` if so.
		let is_last_sub = match self.server.num_subscriptions.entry(subscription_state.raw_id.clone()) {
			Entry::Vacant(_) => unreachable!(),
			Entry::Occupied(ref mut e) if e.get().get() >= 2 => {
				let e = e.get_mut();
				*e = NonZeroUsize::new(e.get() - 1).expect("e is >= 2; qed");
				false
			}
			Entry::Occupied(e) => {
				e.remove();
				true
			}
		};

		// If the subscription is pending, we have yet to send something back on that connection
		// and thus shouldn't close it.
		// When the response is sent back later, the code will realize that `num_subscriptions`
		// is zero/empty and call `finish`.
		if is_last_sub && !subscription_state.pending {
			let _ = self.server.raw.finish(&subscription_state.raw_id, None).await;
		}
	}
}

impl fmt::Display for IntoSubscriptionErr {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self {
			IntoSubscriptionErr::NotSupported => write!(f, "Underlying server doesn't support subscriptions"),
			IntoSubscriptionErr::Closed => write!(f, "Request is already closed"),
		}
	}
}

impl std::error::Error for IntoSubscriptionErr {}

impl Iterator for SubscriptionsReadyIter {
	type Item = RawServerSubscriptionId;

	fn next(&mut self) -> Option<Self::Item> {
		self.0.next()
	}

	fn size_hint(&self) -> (usize, Option<usize>) {
		self.0.size_hint()
	}
}

impl ExactSizeIterator for SubscriptionsReadyIter {}

impl Iterator for SubscriptionsClosedIter {
	type Item = RawServerSubscriptionId;

	fn next(&mut self) -> Option<Self::Item> {
		self.0.next()
	}

	fn size_hint(&self) -> (usize, Option<usize>) {
		self.0.size_hint()
	}
}

impl ExactSizeIterator for SubscriptionsClosedIter {}
