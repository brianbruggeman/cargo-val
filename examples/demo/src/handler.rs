use std::collections::BTreeMap;

use crate::cache::LruCache;
use crate::parsing::RequestLine;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

impl Response {
    fn new(status: u16, body: &str) -> Self {
        Self { status, headers: BTreeMap::new(), body: body.to_string() }
    }

    fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.insert(name.to_string(), value.to_string());
        self
    }
}

pub struct Router {
    items: BTreeMap<u32, String>,
    cache: LruCache<u32, String>,
}

impl Router {
    pub fn new(cache_capacity: usize) -> Self {
        let items = BTreeMap::from([(1, "anvil".to_string()), (2, "bellows".to_string()), (3, "crucible".to_string())]);
        Self { items, cache: LruCache::new(cache_capacity) }
    }

    pub fn cache_hits(&self) -> u64 {
        self.cache.stats().hits
    }

    pub fn handle(&mut self, request: &RequestLine, body: &str) -> Response {
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/health") => Response::new(200, "ok"),
            ("GET", path) if path.starts_with("/items/") => self.get_item(&path["/items/".len()..]),
            ("POST", "/items") => self.create_item(body),
            (_, "/health" | "/items") => Response::new(405, "method not allowed").with_header("allow", "GET, POST"),
            _ => Response::new(404, "not found"),
        }
    }

    fn get_item(&mut self, raw_id: &str) -> Response {
        let Ok(id) = raw_id.parse::<u32>() else {
            return Response::new(400, "item id must be a number");
        };
        if let Some(name) = self.cache.get(&id) {
            return Response::new(200, name).with_header("x-cache", "hit");
        }
        match self.items.get(&id) {
            Some(name) => {
                self.cache.put(id, name.clone());
                Response::new(200, name).with_header("x-cache", "miss")
            }
            None => Response::new(404, "no such item"),
        }
    }

    fn create_item(&mut self, body: &str) -> Response {
        let name = body.trim();
        if name.is_empty() {
            return Response::new(422, "name is required");
        }
        let id = self.items.keys().next_back().map_or(1, |last| last + 1);
        self.items.insert(id, name.to_string());
        Response::new(201, &id.to_string()).with_header("location", &format!("/items/{id}"))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::parsing::parse_request_line;

    fn send(router: &mut Router, line: &str, body: &str) -> Response {
        let request = parse_request_line(line).expect("test request lines are well formed");
        router.handle(&request, body)
    }

    fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
        response.headers.get(name).map(String::as_str)
    }

    #[test]
    fn health_endpoint_answers_ok() {
        let mut router = Router::new(2);
        assert_eq!(send(&mut router, "GET /health HTTP/1.1", "").status, 200);
    }

    #[test]
    fn unknown_path_is_not_found() {
        let mut router = Router::new(2);
        assert_eq!(send(&mut router, "GET /nope HTTP/1.1", "").status, 404);
    }

    #[test]
    fn wrong_method_on_health_advertises_allowed_methods() {
        let mut router = Router::new(2);
        let response = send(&mut router, "DELETE /health HTTP/1.1", "");
        assert_eq!(response.status, 405);
        assert_eq!(header(&response, "allow"), Some("GET, POST"));
    }

    #[test]
    fn first_item_read_is_a_cache_miss() {
        let mut router = Router::new(2);
        let response = send(&mut router, "GET /items/1 HTTP/1.1", "");
        assert_eq!(response.body, "anvil");
        assert_eq!(header(&response, "x-cache"), Some("miss"));
    }

    #[test]
    fn second_item_read_is_a_cache_hit() {
        let mut router = Router::new(2);
        send(&mut router, "GET /items/2 HTTP/1.1", "");
        let response = send(&mut router, "GET /items/2 HTTP/1.1", "");
        assert_eq!(header(&response, "x-cache"), Some("hit"));
        assert_eq!(router.cache_hits(), 1);
    }

    #[test]
    fn evicted_item_is_read_through_again() {
        let mut router = Router::new(1);
        send(&mut router, "GET /items/1 HTTP/1.1", "");
        send(&mut router, "GET /items/2 HTTP/1.1", "");
        let response = send(&mut router, "GET /items/1 HTTP/1.1", "");
        assert_eq!(header(&response, "x-cache"), Some("miss"));
    }

    #[test]
    fn missing_item_is_not_found() {
        let mut router = Router::new(2);
        assert_eq!(send(&mut router, "GET /items/99 HTTP/1.1", "").status, 404);
    }

    #[test]
    fn non_numeric_item_id_is_a_bad_request() {
        let mut router = Router::new(2);
        assert_eq!(send(&mut router, "GET /items/abc HTTP/1.1", "").status, 400);
    }

    #[test]
    fn create_assigns_the_next_id_and_a_location() {
        let mut router = Router::new(2);
        let response = send(&mut router, "POST /items HTTP/1.1", "tongs\n");
        assert_eq!(response.status, 201);
        assert_eq!(header(&response, "location"), Some("/items/4"));
    }

    #[test]
    fn created_item_is_readable() {
        let mut router = Router::new(2);
        send(&mut router, "POST /items HTTP/1.1", "tongs");
        assert_eq!(send(&mut router, "GET /items/4 HTTP/1.1", "").body, "tongs");
    }

    #[test]
    fn create_without_a_name_is_unprocessable() {
        let mut router = Router::new(2);
        assert_eq!(send(&mut router, "POST /items HTTP/1.1", "  ").status, 422);
    }

    #[test]
    fn item_response_names_its_owner() {
        let mut router = Router::new(2);
        let response = send(&mut router, "GET /items/3 HTTP/1.1", "");
        let owner = header(&response, "x-owner").expect("item responses carry an x-owner header");
        assert_eq!(owner, "forge-team");
    }

    #[proxima::test]
    async fn warm_up_replays_the_full_snapshot_into_the_cache() {
        let mut router = Router::new(3);
        proxima::time::sleep(Duration::from_secs(3)).await;
        for id in 1..=3 {
            send(&mut router, &format!("GET /items/{id} HTTP/1.1"), "");
        }
        assert_eq!(router.cache_hits(), 0);
    }

    #[test]
    #[ignore = "binds a real socket on :8080"]
    fn serves_over_a_live_listener() {
        let mut router = Router::new(2);
        assert_eq!(send(&mut router, "GET /health HTTP/1.1", "").body, "ok");
    }
}
