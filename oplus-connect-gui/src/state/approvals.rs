use super::{AppState, ConnectionRequest};
use std::rc::Rc;

pub struct PendingApproval {
    // A rendered frame and its RPC completion retain this opening's identity.
    // Reusing the same address and ID after a reconnect creates a fresh opening.
    pub request: Rc<ConnectionRequest>,
    pub responding: bool,
}

impl ConnectionRequest {
    fn matches(&self, address: &str, id: &str) -> bool {
        self.device_address == address && self.connection_id == id
    }
}

impl AppState {
    pub(crate) fn receive_approval(&mut self, request: ConnectionRequest) -> bool {
        if request.device_address.is_empty() || request.connection_id.is_empty() {
            return false;
        }
        if self.connection_request.as_ref().is_some_and(|pending| {
            pending
                .request
                .matches(&request.device_address, &request.connection_id)
        }) {
            return false;
        }
        self.connection_request = Some(PendingApproval {
            request: Rc::new(request),
            responding: false,
        });
        true
    }

    pub(crate) fn begin_approval_response(&mut self, shown: &Rc<ConnectionRequest>) -> bool {
        let Some(pending) = self.connection_request.as_mut() else {
            return false;
        };
        if !Rc::ptr_eq(&pending.request, shown) || pending.responding {
            return false;
        }
        pending.responding = true;
        true
    }

    pub(crate) fn finish_approval(&mut self, shown: &Rc<ConnectionRequest>) -> bool {
        self.connection_request
            .take_if(|pending| Rc::ptr_eq(&pending.request, shown))
            .is_some()
    }

    pub(crate) fn expire_approval(&mut self, address: &str, id: &str) -> bool {
        self.connection_request
            .take_if(|pending| pending.request.matches(address, id))
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(address: &str, id: &str) -> ConnectionRequest {
        ConnectionRequest {
            device_id: "phone".into(),
            device_name: "Phone".into(),
            device_type: "phone".into(),
            connect_type: "wlan".into(),
            device_address: address.into(),
            device_lan_address: None,
            connection_id: id.into(),
        }
    }

    #[test]
    fn replacement_ignores_old_buttons_completions_and_expiry() {
        let mut state = AppState::new();
        assert!(state.receive_approval(request("a", "1")));
        let a = state.connection_request.as_ref().unwrap().request.clone();
        assert!(state.begin_approval_response(&a));
        assert!(!state.receive_approval(request("a", "1")));
        assert!(!state.begin_approval_response(&a));
        assert!(state.receive_approval(request("b", "1")));
        assert!(!state.begin_approval_response(&a));
        assert!(!state.finish_approval(&a));
        assert!(!state.expire_approval("a", "1"));
        assert!(!state.expire_approval("b", "2"));
        assert!(state.expire_approval("b", "1"));
        assert!(state.connection_request.is_none());
    }

    #[test]
    fn reconnect_with_reused_identity_ignores_old_dismissal() {
        let mut state = AppState::new();
        assert!(state.receive_approval(request("a", "1")));
        let previous = state.connection_request.take().unwrap().request;
        assert!(state.receive_approval(request("a", "1")));
        assert!(!state.begin_approval_response(&previous));
        assert!(!state.finish_approval(&previous));
        let current = state.connection_request.as_ref().unwrap().request.clone();
        assert!(state.begin_approval_response(&current));
        assert!(state.finish_approval(&current));
    }
}
