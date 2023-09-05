//! Client connection state
use crate::error::Error;
use crate::error::Result;
use crate::protocol::Close;
use crate::protocol::Event;

use crate::protocol::{Subscription, SubscriptionId};
use log::*;
use std::collections::HashMap;
use uuid::Uuid;

/// A subscription identifier has a maximum length
const MAX_SUBSCRIPTION_ID_LEN: usize = 256;

/// State for a client connection
pub struct ClientConn {
    /// Unique client identifier generated at connection time
    client_id: Uuid,
    /// The current set of active client subscriptions
    subscriptions: HashMap<SubscriptionId, Subscription>,
    /// Per-connection maximum concurrent subscriptions
    max_subs: usize,
}

impl Default for ClientConn {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientConn {
    /// Create a new, empty connection state.
    pub fn new() -> Self {
        let client_id = Uuid::new_v4();
        ClientConn {
            client_id,
            subscriptions: HashMap::new(),
            max_subs: 32,
        }
    }

    /// Get a short prefix of the client's unique identifier, suitable
    /// for logging.
    pub fn get_client_prefix(&self) -> String {
        self.client_id.to_string().chars().take(8).collect()
    }

    /// Find all matching subscriptions.
    pub fn get_matching_subscriptions(&self, e: &Event) -> Vec<&SubscriptionId> {
        let mut v: Vec<&SubscriptionId> = vec![];
        for (id, sub) in self.subscriptions.iter() {
            if sub.interested_in_event(e) {
                v.push(id);
            }
        }
        v
    }

    /// Add a new subscription for this connection.
    pub fn subscribe(&mut self, s: Subscription) -> Result<()> {
        let subs_id = s.get_id().clone();
        let sub_id_len = subs_id.len();
        // prevent arbitrarily long subscription identifiers from
        // being used.
        if sub_id_len > MAX_SUBSCRIPTION_ID_LEN {
            info!(
                "ignoring sub request with excessive length: ({})",
                sub_id_len
            );
            return Err(Error::SubIdMaxLengthError);
        }
        // check if an existing subscription exists, and replace if so
        if self.subscriptions.contains_key(&subs_id) {
            self.subscriptions.remove(&subs_id);
            self.subscriptions.insert(subs_id, s);
            debug!("replaced existing subscription");
            return Ok(());
        }

        // check if there is room for another subscription.
        if self.subscriptions.len() >= self.max_subs {
            return Err(Error::SubMaxExceededError);
        }
        // add subscription
        self.subscriptions.insert(subs_id, s);
        debug!(
            "registered new subscription, currently have {} active subs",
            self.subscriptions.len()
        );
        Ok(())
    }

    /// Remove the subscription for this connection.
    pub fn unsubscribe(&mut self, c: Close) {
        // TODO: return notice if subscription did not exist.
        self.subscriptions.remove(&c.id);
        debug!(
            "removed subscription, currently have {} active subs",
            self.subscriptions.len()
        );
    }
}
