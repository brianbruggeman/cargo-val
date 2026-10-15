#[test]
fn a_health_check_is_routed() {
    assert_eq!(eta_app::handle("GET /health"), "health");
}

#[test]
fn an_unknown_verb_is_rejected() {
    assert_eq!(eta_app::handle("PUT /health"), "rejected");
}
