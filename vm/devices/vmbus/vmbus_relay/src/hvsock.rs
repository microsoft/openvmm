// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use vmbus_core::protocol;
use vmbus_core::HvsockConnectRequest;
use vmbus_core::HvsockConnectResult;

/// Tracks guest-to-host hvsocket requests that the host has not responded toyet.
pub struct HvsockRequestTracker {
    pending_requests: Vec<HvsockConnectRequest>,
}

impl HvsockRequestTracker {
    /// Create a new request tracker.
    pub fn new() -> Self {
        Self {
            pending_requests: Vec::new(),
        }
    }

    /// Adds a new request to be tracked.
    pub fn add_request(&mut self, request: HvsockConnectRequest) {
        self.pending_requests.push(request);
    }

    /// Checks if a result from the host matches a request, and if so removes it.
    pub fn check_result(&mut self, result: &HvsockConnectResult) {
        if let Some(index) = self.pending_requests.iter().position(|request| {
            request.service_id == result.service_id && request.endpoint_id == result.endpoint_id
        }) {
            self.pending_requests.swap_remove(index);
        } else {
            tracing::warn!(?result, "Result for unknown hvsock request");
        }
    }

    /// Checks if an offer from the host matches a request, and if so removes it and returns a
    /// result message to send to the vmbus server.
    pub fn check_offer(&mut self, offer: &protocol::OfferChannel) -> Option<HvsockConnectResult> {
        if !offer.flags.tlnpi_provider() {
            return None;
        }

        let params = offer.user_defined.as_hvsock_params();
        if params.is_for_guest_accept != 0 {
            return None;
        }

        // Since silo_id isn't part of the result message, it doesn't need to be checked here
        // either.
        let Some(index) = self.pending_requests.iter().position(|request| {
            request.service_id == offer.interface_id && request.endpoint_id == offer.instance_id
        }) else {
            tracing::warn!(?offer, "Channel offer for unknown hvsock request");
            return None;
        };

        let request = self.pending_requests.swap_remove(index);
        tracing::debug!(?request, "channel offer matches hvsocket request");
        Some(HvsockConnectResult::from_request(&request, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vmbus_core::protocol::HvsockUserDefinedParameters;
    use vmbus_core::protocol::OfferFlags;
    use vmbus_core::protocol::UserDefinedData;
    use vmbus_server::Guid;
    use zerocopy::FromZeroes;

    struct FakeRpcSender<T>(Option<T>);

    impl<T> RpcSend for &mut FakeRpcSender<T> {
        type Message = T;

        fn send_rpc(self, message: Self::Message) {
            self.0 = Some(message);
        }
    }

    fn dummy_rpc<T: 'static + Send, U: 'static + Send>(t: T) -> Rpc<T, U> {
        let mut sender = FakeRpcSender(None);
        let _ = sender.call(|x| x, t);
        sender.0.unwrap()
    }

    #[test]
    fn test_check_result() {
        let mut tracker = HvsockRequestTracker::new();
        let request = HvsockConnectRequest {
            service_id: Guid::new_random(),
            endpoint_id: Guid::new_random(),
            silo_id: Guid::new_random(),
        };

        tracker.add_request(request);
        assert_eq!(1, tracker.pending_requests.len());

        // Endpoint ID mismatch.
        let result = HvsockConnectResult {
            service_id: request.service_id,
            endpoint_id: Guid::new_random(),
            success: false,
        };

        tracker.check_result(&result);
        assert_eq!(1, tracker.pending_requests.len());

        // Service ID mismatch.
        let result = HvsockConnectResult {
            service_id: Guid::new_random(),
            endpoint_id: request.endpoint_id,
            success: false,
        };

        tracker.check_result(&result);
        assert_eq!(1, tracker.pending_requests.len());

        // Match.
        let result = HvsockConnectResult::from_request(&request, false);
        tracker.check_result(&result);
        assert_eq!(0, tracker.pending_requests.len());
    }

    #[test]
    fn test_check_offer() {
        let mut tracker = HvsockRequestTracker::new();
        let request = HvsockConnectRequest {
            service_id: Guid::new_random(),
            endpoint_id: Guid::new_random(),
            silo_id: Guid::new_random(),
        };

        tracker.add_request(request);
        assert_eq!(1, tracker.pending_requests.len());

        // Endpoint ID mismatch.
        let offer = create_offer(request.service_id, Guid::new_random(), true, false);
        assert!(tracker.check_offer(&offer).is_none());

        // Endpoint ID mismatch.
        let offer = create_offer(Guid::new_random(), request.endpoint_id, true, false);
        assert!(tracker.check_offer(&offer).is_none());

        // Not a socket request.
        let offer = create_offer(request.service_id, request.endpoint_id, false, false);
        assert!(tracker.check_offer(&offer).is_none());

        // Accept request.
        let offer = create_offer(request.service_id, request.endpoint_id, true, true);
        assert!(tracker.check_offer(&offer).is_none());

        // Match.
        let offer = create_offer(request.service_id, request.endpoint_id, true, false);
        let found = tracker.check_offer(&offer).unwrap();
        assert_eq!(found, HvsockConnectResult::from_request(&request, true));
        assert_eq!(0, tracker.pending_requests.len());

        // It no longer exists.
        let offer = create_offer(request.service_id, request.endpoint_id, true, false);
        assert!(tracker.check_offer(&offer).is_none());
    }

    fn create_offer(
        interface_id: Guid,
        instance_id: Guid,
        hvsock: bool,
        is_for_guest_accept: bool,
    ) -> protocol::OfferChannel {
        let mut user_defined = UserDefinedData::new_zeroed();
        *user_defined.as_hvsock_params_mut() =
            HvsockUserDefinedParameters::new(is_for_guest_accept, true, Guid::new_random());

        protocol::OfferChannel {
            interface_id,
            instance_id,
            flags: OfferFlags::new()
                .with_enumerate_device_interface(true)
                .with_named_pipe_mode(true)
                .with_tlnpi_provider(hvsock),
            user_defined,
            ..FromZeroes::new_zeroed()
        }
    }
}
