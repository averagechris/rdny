use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Outbound {
    pub client_id: u64,
    pub value: Value,
}

#[derive(Debug)]
pub(crate) struct Multiplexer {
    next_upstream_id: u64,
    max_queue: usize,
    clients: HashMap<u64, Client>,
    upstream: HashMap<u64, Route>,
    sessions: HashMap<String, u64>,
}
#[derive(Debug)]
struct Client {
    queue: VecDeque<Value>,
    owned_sessions: Vec<String>,
}
#[derive(Debug, Clone)]
struct Route {
    client_id: u64,
    client_request_id: u64,
    session_id: Option<String>,
}

impl Multiplexer {
    pub(crate) fn new(max_queue: usize) -> Self {
        Self {
            next_upstream_id: 1,
            max_queue,
            clients: HashMap::new(),
            upstream: HashMap::new(),
            sessions: HashMap::new(),
        }
    }
    pub(crate) fn attach_client(&mut self, client_id: u64) {
        self.clients.entry(client_id).or_insert_with(|| Client {
            queue: VecDeque::new(),
            owned_sessions: Vec::new(),
        });
    }
    pub(crate) fn disconnect_client(&mut self, client_id: u64) {
        if let Some(c) = self.clients.remove(&client_id) {
            for s in c.owned_sessions {
                self.sessions.remove(&s);
            }
        }
        self.upstream.retain(|_, r| r.client_id != client_id);
    }
    pub(crate) fn client_request(&mut self, client_id: u64, mut value: Value) -> Result<Value> {
        if !self.clients.contains_key(&client_id) {
            bail!("unknown client");
        }
        let client_request_id = value
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("CDP request missing numeric id"))?;
        if let Some(s) = value.get("sessionId").and_then(Value::as_str)
            && self.sessions.get(s).copied() != Some(client_id)
        {
            bail!("client does not own session");
        }
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
                self.learn_session(&route, &value);
                return self.enqueue(route.client_id, value).map(Some);
            }
            return Ok(None);
        }
        if let Some(s) = value.get("sessionId").and_then(Value::as_str)
            && let Some(client_id) = self.sessions.get(s).copied()
        {
            return self.enqueue(client_id, value).map(Some);
        }
        Ok(None)
    }
    fn learn_session(&mut self, route: &Route, value: &Value) {
        if let Some(sid) = value.pointer("/result/sessionId").and_then(Value::as_str) {
            self.sessions.insert(sid.to_string(), route.client_id);
            if let Some(c) = self.clients.get_mut(&route.client_id) {
                c.owned_sessions.push(sid.to_string());
            }
        }
    }
    fn enqueue(&mut self, client_id: u64, value: Value) -> Result<Outbound> {
        let c = self
            .clients
            .get_mut(&client_id)
            .ok_or_else(|| anyhow::anyhow!("late message for disconnected client"))?;
        if c.queue.len() >= self.max_queue {
            bail!("client queue limit exceeded");
        }
        c.queue.push_back(value.clone());
        Ok(Outbound { client_id, value })
    }
    pub(crate) fn pop_client(&mut self, client_id: u64) -> Option<Value> {
        self.clients.get_mut(&client_id)?.queue.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rewrites_request_ids_and_routes_responses() {
        let mut m = Multiplexer::new(8);
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
        let mut m = Multiplexer::new(8);
        m.attach_client(1);
        let _ = m
            .client_request(1, json!({"id": 1, "method": "X"}))
            .unwrap();
        m.disconnect_client(1);
        assert!(
            m.upstream_message(json!({"id": 1, "result": {}}))
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn slow_client_queue_is_bounded() {
        let mut m = Multiplexer::new(1);
        m.attach_client(1);
        let _ = m
            .client_request(1, json!({"id": 1, "method": "X"}))
            .unwrap();
        m.upstream_message(json!({"id": 1, "result": {"sessionId": "s"}}))
            .unwrap();
        assert!(
            m.upstream_message(json!({"sessionId":"s", "method":"E"}))
                .is_err()
        );
    }
}
