use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outbound {
    pub client_id: u64,
    pub value: Value,
}

#[derive(Debug)]
pub(crate) struct Multiplexer {
    next_upstream_id: u64,
    clients: HashMap<u64, Client>,
    upstream: HashMap<u64, Route>,
    sessions: HashMap<String, u64>,
    session_parents: HashMap<String, String>,
    late_attach: HashSet<u64>,
}
#[derive(Debug)]
struct Client {
    owned_sessions: Vec<String>,
}
#[derive(Debug, Clone)]
struct Route {
    client_id: u64,
    client_request_id: u64,
    session_id: Option<String>,
    session_action: SessionAction,
}

#[derive(Debug, Clone)]
enum SessionAction {
    None,
    Attach,
    Detach(String),
}

impl Multiplexer {
    pub(crate) fn new() -> Self {
        Self {
            next_upstream_id: 1,
            clients: HashMap::new(),
            upstream: HashMap::new(),
            sessions: HashMap::new(),
            session_parents: HashMap::new(),
            late_attach: HashSet::new(),
        }
    }
    pub(crate) fn attach_client(&mut self, client_id: u64) {
        self.clients.entry(client_id).or_insert_with(|| Client {
            owned_sessions: Vec::new(),
        });
    }
    pub(crate) fn disconnect_client(&mut self, client_id: u64) -> Vec<String> {
        let mut detached = Vec::new();
        if let Some(c) = self.clients.remove(&client_id) {
            for s in c.owned_sessions {
                self.release_session_tree(&s);
                detached.push(s);
            }
        }
        let pending: Vec<_> = self
            .upstream
            .iter()
            .filter(|(_, route)| route.client_id == client_id)
            .map(|(id, route)| (*id, matches!(route.session_action, SessionAction::Attach)))
            .collect();
        for (id, attaches_target) in pending {
            self.upstream.remove(&id);
            if attaches_target {
                self.late_attach.insert(id);
            }
        }
        detached
    }
    pub(crate) fn client_request(&mut self, client_id: u64, mut value: Value) -> Result<Value> {
        if !self.clients.contains_key(&client_id) {
            bail!("unknown client");
        }
        let client_request_id = value
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("CDP request missing numeric id"))?;
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .filter(|method| !method.is_empty())
            .ok_or_else(|| anyhow::anyhow!("CDP request missing non-empty method"))?;
        let top_level_session = match value.get("sessionId") {
            None => None,
            Some(Value::String(session)) if !session.is_empty() => {
                self.require_owner(client_id, session)?;
                Some(session.clone())
            }
            Some(_) => bail!("CDP top-level sessionId must be a non-empty string"),
        };
        let params_session = value
            .get("params")
            .and_then(|params| params.get("sessionId"));
        let session_action = match method {
            "Target.detachFromTarget" => {
                if top_level_session.is_some() {
                    bail!("Target.detachFromTarget must be browser-scoped");
                }
                let session = validate_target_session_params(&value, method, false)?;
                self.require_owner(client_id, &session)?;
                SessionAction::Detach(session)
            }
            "Target.sendMessageToTarget" => {
                if top_level_session.is_some() {
                    bail!("Target.sendMessageToTarget must be browser-scoped");
                }
                let session = validate_target_session_params(&value, method, true)?;
                self.require_owner(client_id, &session)?;
                SessionAction::None
            }
            "Target.setAutoAttach" | "Target.autoAttachRelated" => bail!(
                "{method} is not supported through the broker because it can create sessions without an attributable client owner"
            ),
            "Target.attachToTarget" | "Target.attachToBrowserTarget" => SessionAction::Attach,
            "Page.screencastFrameAck" => {
                if params_session.is_some_and(|session| session.as_u64().is_none()) {
                    bail!("Page.screencastFrameAck params.sessionId must be an unsigned integer");
                }
                SessionAction::None
            }
            _ if params_session.is_some() => bail!(
                "unsupported browser-scoped sessionId in params for {method}; use the flat top-level sessionId or an explicitly supported Target method"
            ),
            _ => SessionAction::None,
        };
        let upstream_id = self.next_upstream_id;
        self.next_upstream_id += 1;
        value["id"] = json!(upstream_id);
        let session_id = value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        self.upstream.insert(
            upstream_id,
            Route {
                client_id,
                client_request_id,
                session_id,
                session_action,
            },
        );
        Ok(value)
    }
    pub(crate) fn upstream_message(&mut self, mut value: Value) -> Result<Option<Outbound>> {
        if let Some(id) = value.get("id").and_then(Value::as_u64) {
            if let Some(route) = self.upstream.remove(&id) {
                value["id"] = json!(route.client_request_id);
                if value.get("sessionId").is_none()
                    && let Some(s) = &route.session_id
                {
                    value["sessionId"] = json!(s);
                }
                self.apply_session_action(&route, &value);
                return Ok(Some(Outbound {
                    client_id: route.client_id,
                    value,
                }));
            }
            return Ok(None);
        }
        if let Some(s) = value.get("sessionId").and_then(Value::as_str) {
            if let Some(client_id) = self.sessions.get(s).copied() {
                self.learn_nested_attached_session(client_id, &value);
                if value.get("method").and_then(Value::as_str) == Some("Target.detachedFromTarget")
                    && let Some(detached) =
                        value.pointer("/params/sessionId").and_then(Value::as_str)
                {
                    self.release_session_tree(detached);
                }
                return Ok(Some(Outbound { client_id, value }));
            }
            return Ok(None);
        }
        if matches!(
            value.get("method").and_then(Value::as_str),
            Some("Target.receivedMessageFromTarget" | "Target.detachedFromTarget")
        ) && let Some(session) = value.pointer("/params/sessionId").and_then(Value::as_str)
            && let Some(client_id) = self.sessions.get(session).copied()
        {
            if value.get("method").and_then(Value::as_str) == Some("Target.detachedFromTarget") {
                self.release_session_tree(session);
            }
            return Ok(Some(Outbound { client_id, value }));
        }
        Ok(None)
    }
    fn apply_session_action(&mut self, route: &Route, value: &Value) {
        if value.get("error").is_some() {
            return;
        }
        match &route.session_action {
            SessionAction::Attach => {
                if let Some(sid) = value.pointer("/result/sessionId").and_then(Value::as_str) {
                    self.assign_session(route.client_id, sid, route.session_id.as_deref());
                }
            }
            SessionAction::Detach(session) => self.release_session_tree(session),
            SessionAction::None => {}
        }
    }

    fn require_owner(&self, client_id: u64, session: &str) -> Result<()> {
        if self.sessions.get(session).copied() != Some(client_id) {
            bail!("client does not own session");
        }
        Ok(())
    }

    fn assign_session(&mut self, client_id: u64, session: &str, parent: Option<&str>) {
        if let Some(old_client) = self.sessions.get(session).copied()
            && old_client != client_id
            && let Some(client) = self.clients.get_mut(&old_client)
        {
            client.owned_sessions.retain(|owned| owned != session);
        }
        self.sessions.insert(session.to_string(), client_id);
        if let Some(parent) = parent {
            self.session_parents
                .insert(session.to_string(), parent.to_string());
        }
        if let Some(client) = self.clients.get_mut(&client_id)
            && !client.owned_sessions.iter().any(|owned| owned == session)
        {
            client.owned_sessions.push(session.to_string());
        }
    }

    fn release_session_tree(&mut self, session: &str) {
        let children: Vec<String> = self
            .session_parents
            .iter()
            .filter(|(_, parent)| parent.as_str() == session)
            .map(|(child, _)| child.clone())
            .collect();
        for child in children {
            self.release_session_tree(&child);
        }
        self.session_parents.remove(session);
        if let Some(client_id) = self.sessions.remove(session)
            && let Some(client) = self.clients.get_mut(&client_id)
        {
            client.owned_sessions.retain(|owned| owned != session);
        }
    }

    fn learn_nested_attached_session(&mut self, client_id: u64, value: &Value) {
        if value.get("method").and_then(Value::as_str) == Some("Target.attachedToTarget")
            && let Some(parent) = value.get("sessionId").and_then(Value::as_str)
            && let Some(session) = value.pointer("/params/sessionId").and_then(Value::as_str)
        {
            self.assign_session(client_id, session, Some(parent));
        }
    }
    pub(crate) fn take_late_attached_session(&mut self, value: &Value) -> Option<String> {
        let id = value.get("id")?.as_u64()?;
        if !self.late_attach.remove(&id) {
            return None;
        }
        value
            .pointer("/result/sessionId")?
            .as_str()
            .map(str::to_string)
    }

    pub(crate) fn take_unowned_attached_session(&self, value: &Value) -> Option<String> {
        if value.get("method").and_then(Value::as_str) != Some("Target.attachedToTarget") {
            return None;
        }
        if value
            .get("sessionId")
            .and_then(Value::as_str)
            .is_some_and(|parent| self.sessions.contains_key(parent))
        {
            return None;
        }
        let session = value.pointer("/params/sessionId")?.as_str()?;
        (!self.sessions.contains_key(session)).then(|| session.to_string())
    }
}

fn validate_target_session_params(
    value: &Value,
    method: &str,
    message_required: bool,
) -> Result<String> {
    let params = value
        .get("params")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("{method} params must be an object"))?;
    let session = params
        .get("sessionId")
        .and_then(Value::as_str)
        .filter(|session| !session.is_empty())
        .ok_or_else(|| anyhow::anyhow!("{method} params.sessionId must be a non-empty string"))?;
    if params.contains_key("targetId") {
        bail!("{method} targetId addressing is not supported; an owned sessionId is required");
    }
    if message_required
        && params
            .get("message")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        bail!("{method} params.message must be a non-empty string");
    }
    let allowed = if message_required {
        ["sessionId", "message"].as_slice()
    } else {
        ["sessionId"].as_slice()
    };
    if let Some(key) = params.keys().find(|key| !allowed.contains(&key.as_str())) {
        bail!("{method} contains unsupported parameter `{key}`");
    }
    Ok(session.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_owned_session() -> Multiplexer {
        let mut mux = Multiplexer::new();
        mux.attach_client(1);
        mux.attach_client(2);
        let request = mux
            .client_request(1, json!({"id":1,"method":"Target.attachToTarget","params":{"targetId":"t","flatten":true}}))
            .unwrap();
        mux.upstream_message(json!({"id":request["id"],"result":{"sessionId":"owned"}}))
            .unwrap();
        mux
    }
    #[test]
    fn rewrites_request_ids_and_routes_responses() {
        let mut m = Multiplexer::new();
        m.attach_client(42);
        let up = m
            .client_request(42, json!({"id": 9, "method": "Target.attachToTarget"}))
            .unwrap();
        assert_eq!(up["id"], 1);
        let out = m
            .upstream_message(json!({"id": 1, "result": {"sessionId": "s1"}}))
            .unwrap()
            .unwrap();
        assert_eq!(out.client_id, 42);
        assert_eq!(out.value["id"], 9);
        let ev = m
            .upstream_message(json!({"sessionId": "s1", "method": "Runtime.consoleAPICalled"}))
            .unwrap()
            .unwrap();
        assert_eq!(ev.client_id, 42);
    }
    #[test]
    fn detach_on_disconnect_and_late_cleanup() {
        let mut m = Multiplexer::new();
        m.attach_client(1);
        let _ = m
            .client_request(1, json!({"id": 1, "method": "X"}))
            .unwrap();
        let _ = m.disconnect_client(1);
        assert!(
            m.upstream_message(json!({"id": 1, "result": {}}))
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn attached_sessions_are_owned_by_one_concurrent_client() {
        let mut m = Multiplexer::new();
        m.attach_client(1);
        m.attach_client(2);
        let request = m
            .client_request(1, json!({"id":1,"method":"Target.attachToTarget"}))
            .unwrap();
        m.upstream_message(json!({"id":request["id"],"result":{"sessionId":"private"}}))
            .unwrap();
        assert!(
            m.client_request(
                2,
                json!({"id":2,"sessionId":"private","method":"Runtime.evaluate"})
            )
            .is_err()
        );
        assert_eq!(m.disconnect_client(1), vec!["private"]);
    }

    #[test]
    fn attach_to_browser_target_result_is_owned_by_requesting_client() {
        let mut mux = Multiplexer::new();
        mux.attach_client(1);
        mux.attach_client(2);
        let request = mux
            .client_request(
                1,
                json!({"id":1,"method":"Target.attachToBrowserTarget","params":{}}),
            )
            .unwrap();
        mux.upstream_message(json!({"id":request["id"],"result":{"sessionId":"browser-session"}}))
            .unwrap();
        assert!(mux.client_request(1, json!({"id":2,"method":"Runtime.evaluate","sessionId":"browser-session","params":{}})).is_ok());
        assert!(mux.client_request(2, json!({"id":3,"method":"Runtime.evaluate","sessionId":"browser-session","params":{}})).is_err());
    }

    #[test]
    fn late_attach_response_is_marked_for_detach_after_disconnect() {
        let mut m = Multiplexer::new();
        m.attach_client(1);
        let request = m
            .client_request(1, json!({"id":7,"method":"Target.attachToTarget"}))
            .unwrap();
        let upstream_id = request["id"].as_u64().unwrap();
        m.disconnect_client(1);
        let response = json!({"id":upstream_id,"result":{"sessionId":"late"}});
        assert_eq!(
            m.take_late_attached_session(&response).as_deref(),
            Some("late")
        );
        assert!(m.upstream_message(response).unwrap().is_none());
    }

    #[test]
    fn flat_top_level_session_requires_a_well_formed_owned_id() {
        let mut mux = with_owned_session();
        assert!(
            mux.client_request(
                1,
                json!({"id":2,"method":"Runtime.evaluate","sessionId":"owned","params":{}})
            )
            .is_ok()
        );
        assert!(
            mux.client_request(
                2,
                json!({"id":3,"method":"Runtime.evaluate","sessionId":"owned","params":{}})
            )
            .is_err()
        );
        for malformed in [Value::Null, json!(7), json!(""), json!({})] {
            assert!(
                mux.client_request(
                    1,
                    json!({"id":4,"method":"Runtime.evaluate","sessionId":malformed,"params":{}})
                )
                .is_err()
            );
        }
    }

    #[test]
    fn detach_params_require_ownership_and_release_only_on_success() {
        let mut mux = with_owned_session();
        assert!(
            mux.client_request(
                2,
                json!({"id":2,"method":"Target.detachFromTarget","params":{"sessionId":"owned"}})
            )
            .is_err()
        );
        let request = mux
            .client_request(
                1,
                json!({"id":3,"method":"Target.detachFromTarget","params":{"sessionId":"owned"}}),
            )
            .unwrap();
        assert!(
            mux.client_request(
                1,
                json!({"id":4,"method":"Runtime.evaluate","sessionId":"owned","params":{}})
            )
            .is_ok()
        );
        mux.upstream_message(json!({"id":request["id"],"result":{}}))
            .unwrap();
        assert!(
            mux.client_request(
                1,
                json!({"id":5,"method":"Runtime.evaluate","sessionId":"owned","params":{}})
            )
            .is_err()
        );

        let mut mux = with_owned_session();
        let request = mux
            .client_request(
                1,
                json!({"id":6,"method":"Target.detachFromTarget","params":{"sessionId":"owned"}}),
            )
            .unwrap();
        mux.upstream_message(json!({"id":request["id"],"error":{"code":-1}}))
            .unwrap();
        assert!(
            mux.client_request(
                1,
                json!({"id":7,"method":"Runtime.evaluate","sessionId":"owned","params":{}})
            )
            .is_ok()
        );

        for params in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"sessionId":null}),
            json!({"sessionId":9}),
            json!({"sessionId":""}),
            json!({"targetId":"t"}),
            json!({"sessionId":"owned","targetId":"t"}),
            json!({"sessionId":"owned","extra":true}),
        ] {
            assert!(
                with_owned_session()
                    .client_request(
                        1,
                        json!({"id":8,"method":"Target.detachFromTarget","params":params})
                    )
                    .is_err()
            );
        }
    }

    #[test]
    fn send_message_params_require_owned_session_and_strict_shape() {
        let valid = json!({"id":2,"method":"Target.sendMessageToTarget","params":{"sessionId":"owned","message":"{}"}});
        assert!(
            with_owned_session()
                .client_request(1, valid.clone())
                .is_ok()
        );
        assert!(
            with_owned_session()
                .client_request(2, valid.clone())
                .is_err()
        );
        for params in [
            json!({}),
            json!({"sessionId":null,"message":"{}"}),
            json!({"sessionId":7,"message":"{}"}),
            json!({"sessionId":"","message":"{}"}),
            json!({"sessionId":"owned"}),
            json!({"sessionId":"owned","message":""}),
            json!({"targetId":"t","message":"{}"}),
            json!({"sessionId":"owned","targetId":"t","message":"{}"}),
            json!({"sessionId":"owned","message":"{}","extra":true}),
        ] {
            assert!(
                with_owned_session()
                    .client_request(
                        1,
                        json!({"id":3,"method":"Target.sendMessageToTarget","params":params})
                    )
                    .is_err()
            );
        }
        assert!(with_owned_session().client_request(1, json!({"id":4,"method":"Target.sendMessageToTarget","sessionId":"owned","params":{"sessionId":"owned","message":"{}"}})).is_err());
    }

    #[test]
    fn unsupported_browser_scoped_session_forms_fail_closed() {
        let mut mux = with_owned_session();
        assert!(
            mux.client_request(
                1,
                json!({"id":2,"method":"Future.method","params":{"sessionId":"owned"}})
            )
            .is_err()
        );
        assert!(
            mux.client_request(
                1,
                json!({"id":2,"method":"Future.method","params":{"sessionId":null}})
            )
            .is_err()
        );
        assert!(mux.client_request(1, json!({"id":3,"method":"Page.screencastFrameAck","sessionId":"owned","params":{"sessionId":42}})).is_ok());
        assert!(mux.client_request(1, json!({"id":3,"method":"Page.screencastFrameAck","sessionId":"owned","params":{"sessionId":"owned"}})).is_err());
        assert!(
            mux.client_request(
                1,
                json!({"id":4,"method":"Target.setAutoAttach","params":{"autoAttach":true}})
            )
            .is_err()
        );
        assert!(
            mux.client_request(
                1,
                json!({"id":5,"method":"Target.autoAttachRelated","params":{"targetId":"t"}})
            )
            .is_err()
        );
    }

    #[test]
    fn browser_scoped_session_events_route_and_update_ownership() {
        let mut mux = with_owned_session();
        let received = mux.upstream_message(json!({"method":"Target.receivedMessageFromTarget","params":{"sessionId":"owned","message":"{}"}})).unwrap().unwrap();
        assert_eq!(received.client_id, 1);
        let detached = mux
            .upstream_message(
                json!({"method":"Target.detachedFromTarget","params":{"sessionId":"owned"}}),
            )
            .unwrap()
            .unwrap();
        assert_eq!(detached.client_id, 1);
        assert!(
            mux.client_request(
                1,
                json!({"id":8,"method":"Runtime.evaluate","sessionId":"owned","params":{}})
            )
            .is_err()
        );
    }

    #[test]
    fn nested_attach_inherits_parent_owner_and_unowned_attach_is_exposed() {
        let mut mux = with_owned_session();
        let event = json!({"method":"Target.attachedToTarget","sessionId":"owned","params":{"sessionId":"nested"}});
        assert!(mux.take_unowned_attached_session(&event).is_none());
        assert_eq!(mux.upstream_message(event).unwrap().unwrap().client_id, 1);
        assert!(
            mux.client_request(
                1,
                json!({"id":9,"method":"Runtime.evaluate","sessionId":"nested","params":{}})
            )
            .is_ok()
        );
        let unowned = json!({"method":"Target.attachedToTarget","params":{"sessionId":"orphan"}});
        assert_eq!(
            mux.take_unowned_attached_session(&unowned).as_deref(),
            Some("orphan")
        );
    }

    #[test]
    fn nested_detach_event_reaches_owner_and_releases_nested_session() {
        let mut mux = with_owned_session();
        let attach = json!({"method":"Target.attachedToTarget","sessionId":"owned","params":{"sessionId":"nested"}});
        assert_eq!(mux.upstream_message(attach).unwrap().unwrap().client_id, 1);
        assert!(
            mux.client_request(
                1,
                json!({"id":10,"method":"Runtime.evaluate","sessionId":"nested","params":{}})
            )
            .is_ok()
        );

        let detach = json!({"method":"Target.detachedFromTarget","sessionId":"owned","params":{"sessionId":"nested"}});
        let routed = mux.upstream_message(detach).unwrap().unwrap();
        assert_eq!(routed.client_id, 1);
        assert_eq!(routed.value["sessionId"], "owned");
        assert_eq!(routed.value["params"]["sessionId"], "nested");

        assert!(
            mux.client_request(
                1,
                json!({"id":11,"method":"Runtime.evaluate","sessionId":"nested","params":{}})
            )
            .is_err()
        );
        assert!(
            mux.client_request(
                1,
                json!({"id":12,"method":"Runtime.evaluate","sessionId":"owned","params":{}})
            )
            .is_ok()
        );
    }

    #[test]
    fn releasing_parent_session_releases_descendants() {
        let mut mux = with_owned_session();
        mux.upstream_message(json!({"method":"Target.attachedToTarget","sessionId":"owned","params":{"sessionId":"child"}})).unwrap();
        mux.upstream_message(json!({"method":"Target.attachedToTarget","sessionId":"child","params":{"sessionId":"grandchild"}})).unwrap();
        assert!(
            mux.client_request(
                1,
                json!({"id":13,"method":"Runtime.evaluate","sessionId":"grandchild","params":{}})
            )
            .is_ok()
        );

        let routed = mux
            .upstream_message(
                json!({"method":"Target.detachedFromTarget","params":{"sessionId":"owned"}}),
            )
            .unwrap()
            .unwrap();
        assert_eq!(routed.client_id, 1);
        for session in ["owned", "child", "grandchild"] {
            assert!(
                mux.client_request(
                    1,
                    json!({"id":14,"method":"Runtime.evaluate","sessionId":session,"params":{}})
                )
                .is_err()
            );
        }
    }

    #[test]
    fn attach_response_in_parent_session_records_parent_and_parent_detach_releases_child() {
        for method in ["Target.attachToTarget", "Target.attachToBrowserTarget"] {
            let mut mux = with_owned_session();
            let request = mux
                .client_request(
                    1,
                    json!({"id":20,"method":method,"sessionId":"owned","params":{"targetId":"child-target","flatten":true}}),
                )
                .unwrap();
            mux.upstream_message(json!({"id":request["id"],"result":{"sessionId":"child-from-response"}}))
                .unwrap();
            assert!(mux.client_request(1, json!({"id":21,"method":"Runtime.evaluate","sessionId":"child-from-response","params":{}})).is_ok());

            let routed = mux
                .upstream_message(json!({"method":"Target.detachedFromTarget","params":{"sessionId":"owned"}}))
                .unwrap()
                .unwrap();
            assert_eq!(routed.client_id, 1);
            assert!(mux.client_request(1, json!({"id":22,"method":"Runtime.evaluate","sessionId":"child-from-response","params":{}})).is_err());
        }
    }

    #[test]
    fn attach_response_does_not_erase_event_learned_parent_in_any_order() {
        // Event first, then browser-scoped response with the same child: the
        // later response has no route parent and must not weaken the event link.
        let mut mux = with_owned_session();
        let request = mux
            .client_request(
                1,
                json!({"id":30,"method":"Target.attachToTarget","params":{"targetId":"child","flatten":true}}),
            )
            .unwrap();
        mux.upstream_message(json!({"method":"Target.attachedToTarget","sessionId":"owned","params":{"sessionId":"child"}}))
            .unwrap();
        mux.upstream_message(json!({"id":request["id"],"result":{"sessionId":"child"}}))
            .unwrap();
        mux.upstream_message(json!({"method":"Target.detachedFromTarget","params":{"sessionId":"owned"}}))
            .unwrap();
        assert!(mux.client_request(1, json!({"id":31,"method":"Runtime.evaluate","sessionId":"child","params":{}})).is_err());

        // Response first in a parent session, then event: both orders keep the
        // child linked to the parent and recursively released.
        let mut mux = with_owned_session();
        let request = mux
            .client_request(
                1,
                json!({"id":32,"method":"Target.attachToTarget","sessionId":"owned","params":{"targetId":"child","flatten":true}}),
            )
            .unwrap();
        mux.upstream_message(json!({"id":request["id"],"result":{"sessionId":"child"}}))
            .unwrap();
        mux.upstream_message(json!({"method":"Target.attachedToTarget","sessionId":"owned","params":{"sessionId":"child"}}))
            .unwrap();
        mux.upstream_message(json!({"method":"Target.detachedFromTarget","params":{"sessionId":"owned"}}))
            .unwrap();
        assert!(mux.client_request(1, json!({"id":33,"method":"Runtime.evaluate","sessionId":"child","params":{}})).is_err());
    }
}
