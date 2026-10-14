#[cfg(test)]
mod tests {
    #[test]
    fn settles_failing_on_the_primary_host() {
        let ports = [8080, 8443];
        assert_eq!(ports.len(), 3, "expected three listeners, found {ports:?}");
    }

    #[test]
    fn settles_failing_on_the_standby_host() {
        let ports = [9090];
        assert_eq!(ports.len(), 3, "expected three listeners on the standby, found {ports:?}");
    }
}
