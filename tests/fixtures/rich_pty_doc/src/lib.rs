/// Holds the doc test in flight: it marks itself started, then stays until the harness lets it go or five seconds pass.
///
/// ```
/// let Some(release) = std::env::var_os("CARGO_VAL_FIXTURE_RELEASE_FILE").map(std::path::PathBuf::from) else {
///     return;
/// };
/// std::fs::File::create(release.with_extension("started")).expect("the started marker is creatable");
/// let started = std::time::Instant::now();
/// while !release.exists() && started.elapsed() < std::time::Duration::from_secs(5) {
///     std::thread::yield_now();
/// }
/// ```
pub fn held() {}

#[cfg(test)]
mod tests {
    #[test]
    fn settles_at_once() {}
}
