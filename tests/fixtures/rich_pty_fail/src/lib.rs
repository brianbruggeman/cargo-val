#[cfg(test)]
mod tests {
    #[test]
    fn settles_passing() {
        assert_eq!(2 + 2, 4);
    }

    #[test]
    fn settles_failing() {
        let ports = [8080, 8443];
        assert_eq!(ports.len(), 3, "expected three listeners, found {ports:?}");
    }
}
