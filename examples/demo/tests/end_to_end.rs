use std::time::Duration;

use tinyserve::handler::Router;
use tinyserve::parsing::parse_request_line;

fn get(router: &mut Router, target: &str) -> (u16, String) {
    let request = parse_request_line(&format!("GET {target} HTTP/1.1")).expect("target forms a request line");
    let response = router.handle(&request, "");
    (response.status, response.body)
}

#[test]
fn a_session_reads_creates_and_rereads_items() {
    let mut router = Router::new(4);
    let request = parse_request_line("POST /items HTTP/1.1").expect("post line parses");
    assert_eq!(router.handle(&request, "tongs").status, 201);
    assert_eq!(get(&mut router, "/items/4"), (200, "tongs".to_string()));
    assert_eq!(get(&mut router, "/items/4").0, 200);
    assert_eq!(router.cache_hits(), 1);
}

#[proxima::test(timeout = "2s")]
async fn upstream_refresh_finishes_inside_its_budget() {
    let mut router = Router::new(4);
    proxima::time::sleep(Duration::from_secs(6)).await;
    assert_eq!(get(&mut router, "/health").0, 200);
}
