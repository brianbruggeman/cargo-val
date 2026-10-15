use std::path::{Path, PathBuf};

mod common;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;

use fixture::fixture_output;

const ESCAPE: u8 = 0x1b;

struct Run {
    status: Option<i32>,
    stderr: Vec<u8>,
    text: String,
}

fn target_directory() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
        PathBuf::from,
    )
}

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/build_error/Cargo.toml")
}

async fn run_build_error() -> Run {
    run_with(&["--lib", "--output", "plain"]).await
}

async fn run_with(arguments: &[&str]) -> Run {
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(manifest())
        .args(arguments)
        .env("NO_COLOR", "1")
        .env("CARGO_TERM_COLOR", "always")
        .env_remove("CARGO_VAL_COLOR")
        .env("CARGO_TARGET_DIR", target_directory().join("build-diagnostics-fixture"));
    let output = fixture_output(&mut command).await;
    Run {
        status: output.status.code(),
        text: String::from_utf8_lossy(&output.stderr).into_owned(),
        stderr: output.stderr,
    }
}

#[proxima::test(timeout = "310s")]
async fn a_type_error_shows_rustcs_diagnostic_and_its_location_in_plain_output() {
    let result = run_build_error().await;
    let location_text = format!("{}:4", Path::new("src").join("lib.rs").display());

    let error = result
        .text
        .find("error[E0308]: mismatched types")
        .unwrap_or_else(|| panic!("rustc's E0308 line is missing:\n{}", result.text));
    let location = result
        .text
        .find(&location_text)
        .unwrap_or_else(|| panic!("the file:line {location_text} of the type error is missing:\n{}", result.text));

    assert!(error < location, "{}", result.text);
}

#[proxima::test(timeout = "310s")]
async fn plain_output_prints_rustcs_diagnostic_once_without_cargos_restating_summary_or_a_trailer() {
    let result = run_build_error().await;

    assert_eq!(result.text.matches("error[E0308]: mismatched types").count(), 1, "{}", result.text);
    assert!(!result.text.contains("error: could not compile"), "{}", result.text);
    assert!(!result.text.contains("cargo-val: cargo test build failed"), "{}", result.text);
}

#[proxima::test(timeout = "310s")]
async fn agent_output_leaves_stderr_empty_because_the_error_block_is_on_stdout() {
    let result = run_with(&["--lib", "--output", "agent"]).await;

    assert_eq!(result.status, Some(3), "{}", result.text);
    assert!(!result.text.contains("E0308") && !result.text.contains("could not compile") && !result.text.contains("cargo test build failed"), "rustc's rendering is not duplicated on stderr:\n{}", result.text);
}

#[proxima::test(timeout = "310s")]
async fn a_failed_build_also_shows_the_warnings_cargo_reported() {
    let result = run_build_error().await;

    assert!(
        result.text.contains("warning: unused doc comment"),
        "{}",
        result.text
    );
}

#[proxima::test(timeout = "310s")]
async fn colour_off_diagnostics_carry_no_escape_bytes() {
    let result = run_build_error().await;

    let escaped = result.stderr.contains(&ESCAPE);

    assert!(result.text.contains("error[E0308]"), "{}", result.text);
    assert!(!escaped, "{}", result.text);
}

#[proxima::test(timeout = "310s")]
async fn coloured_diagnostics_carry_sgr_bytes_and_cargos_restating_summary_is_gone() {
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(manifest())
        .arg("--lib")
        .env_remove("NO_COLOR")
        .env("CARGO_VAL_COLOR", "true")
        .env("CARGO_VAL_OUTPUT", "rich")
        .env("CARGO_TARGET_DIR", target_directory().join("build-diagnostics-coloured-fixture"));
    let output = fixture_output(&mut command).await;
    let text = String::from_utf8_lossy(&output.stderr);

    assert!(output.stderr.contains(&ESCAPE), "colour is on, so rustc's diagnostic carries SGR bytes:\n{text}");
    assert_eq!(text.matches("E0308]").count(), 1, "the diagnostic prints once:\n{text}");
    assert!(!text.contains(": could not compile"), "cargo's summary restates the diagnostics above:\n{text}");
    assert!(!text.contains("cargo-val: cargo test build failed"), "rich ends on EXIT, not a trailer line:\n{text}");
}

#[proxima::test(timeout = "310s")]
async fn a_build_failure_exits_3() {
    let result = run_build_error().await;

    assert_eq!(result.status, Some(3), "{}", result.text);
}

#[proxima::test(timeout = "310s")]
async fn an_unknown_flag_is_an_invocation_error_naming_the_flag_and_exiting_2() {
    let result = run_with(&["--bogus"]).await;

    assert_eq!(result.status, Some(2), "{}", result.text);
    assert!(result.text.contains("--bogus"), "{}", result.text);
    assert!(!result.text.contains("build failed"), "{}", result.text);
    assert!(result.text.contains("refused the invocation"), "{}", result.text);
}

const GOLDEN: &str = "tests/fixtures/build_error_golden";
const NOISE: [&str; 3] = ["Compiling ", "Blocking ", "cargo-val: waiting for "];

fn normalised(text: &str) -> String {
    let kept: Vec<String> = text.lines()
        .filter(|line| !NOISE.iter().any(|prefix| line.trim_start().starts_with(prefix)))
        .map(|line| if line.contains("--> ") || line.starts_with("ERROR    ") || line.starts_with("FAILED   ") { line.replace('\\', "/") } else { line.to_owned() })
        .map(|line| {
            let words: Vec<String> = line
                .split(' ')
                .map(|word| match word.split_once('=') {
                    Some(("wall", _)) => "wall=<n>".to_owned(),
                    Some(("jobs", _)) => "jobs=<n>".to_owned(),
                    _ => word.to_owned(),
                })
                .collect();
            words.join(" ")
        })
        .collect();
    kept.iter().fold(String::new(), |mut joined, line| {
        joined.push_str(line);
        joined.push('\n');
        joined
    })
}

fn golden(name: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(GOLDEN).join(name)).expect("the golden capture of main's output is committed")
}

#[proxima::test(timeout = "310s")]
#[case::plain("plain")]
#[case::agent("agent")]
async fn plain_and_agent_build_error_output_is_byte_identical_to_the_committed_capture(#[case] mode: &str) {
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(manifest())
        .args(["--lib", "--output", mode, "--process", "test"])
        .env_remove("CI")
        .env_remove("CARGO_VAL_COLOR")
        .env("NO_COLOR", "1")
        .env("VISUAL", "code")
        .env("CARGO_TERM_COLOR", "always")
        .env("CARGO_TARGET_DIR", target_directory().join(format!("build-diagnostics-golden-{mode}")));

    let output = fixture_output(&mut command).await;

    assert_eq!(output.status.code(), Some(3), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = normalised(&String::from_utf8(output.stdout).expect("stdout is UTF-8"));
    let stderr = normalised(&String::from_utf8(output.stderr).expect("stderr is UTF-8"));
    assert_eq!(stdout, golden(&format!("{mode}.stdout")));
    assert_eq!(stderr, golden(&format!("{mode}.stderr")));
}

fn dependency_error_workspace(root: &Path) -> PathBuf {
    let dependency = root.join("dep");
    let application = root.join("app");
    std::fs::create_dir_all(dependency.join("src")).expect("the dependency source directory is creatable");
    std::fs::create_dir_all(application.join("src")).expect("the application source directory is creatable");
    std::fs::write(dependency.join("Cargo.toml"), "[package]\nname = \"broken_dep\"\nversion = \"0.1.0\"\nedition = \"2024\"\n").expect("the dependency manifest is writable");
    std::fs::write(dependency.join("src/lib.rs"), "pub fn answer() -> u32 {\n    let scale = 6;\n    \"forty-two\"\n}\n").expect("the dependency source is writable");
    std::fs::write(application.join("Cargo.toml"), "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\nbroken_dep = { path = \"../dep\" }\n").expect("the application manifest is writable");
    std::fs::write(application.join("src/lib.rs"), "pub use broken_dep::answer;\n").expect("the application source is writable");
    application.join("Cargo.toml")
}

fn is_excerpt_marker_row(line: &str) -> bool {
    line.strip_prefix("> ").is_some_and(|rest| rest.trim_start().chars().next().is_some_and(|first| first.is_ascii_digit()))
}

#[proxima::test(timeout = "310s")]
async fn a_span_outside_the_workspace_shows_rustcs_diagnostic_with_no_excerpt_and_still_exits_3() {
    let directory = tempfile::tempdir().expect("tempdir is creatable");
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(dependency_error_workspace(directory.path()))
        .args(["--lib", "--output", "rich"])
        .env_remove("CI")
        .env_remove("CARGO_VAL_COLOR")
        .env("NO_COLOR", "1")
        .env("CARGO_TARGET_DIR", target_directory().join("build-diagnostics-outside-fixture"));

    let output = fixture_output(&mut command).await;

    let text = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(3), "{text}");
    assert!(text.contains("error[E0308]: mismatched types"), "{text}");
    assert!(text.contains(&format!("{}:3:5", Path::new("dep").join("src").join("lib.rs").display())), "the span must be the dependency's, outside the workspace:\n{text}");
    assert_eq!(text.lines().filter(|line| is_excerpt_marker_row(line)).count(), 0, "{text}");
    assert!(!text.contains("source not found"), "{text}");
}

fn nested_member_workspace(root: &Path) -> PathBuf {
    let leaf = root.join("crates/leaf");
    std::fs::create_dir_all(leaf.join("src")).expect("the member source directory is creatable");
    std::fs::write(root.join("Cargo.toml"), "[workspace]\nresolver = \"3\"\nmembers = [\"crates/leaf\"]\n").expect("the workspace manifest is writable");
    std::fs::write(leaf.join("Cargo.toml"), "[package]\nname = \"leaf\"\nversion = \"0.1.0\"\nedition = \"2024\"\n").expect("the member manifest is writable");
    std::fs::write(leaf.join("src/lib.rs"), "pub fn answer() -> u32 {\n    let scale = 6;\n    \"forty-two\"\n}\n").expect("the member source is writable");
    leaf.join("Cargo.toml")
}

#[proxima::test(timeout = "310s")]
async fn a_failing_member_below_the_workspace_root_shows_its_excerpt_with_the_label_in_place_of_rustcs_snippet() {
    let directory = tempfile::tempdir().expect("tempdir is creatable");
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(nested_member_workspace(directory.path()))
        .args(["--lib", "--output", "rich"])
        .env_remove("CI")
        .env_remove("CARGO_VAL_COLOR")
        .env("NO_COLOR", "1")
        .env("CARGO_TARGET_DIR", target_directory().join("build-diagnostics-nested-fixture"));

    let output = fixture_output(&mut command).await;

    let text = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(output.status.code(), Some(3), "{text}");
    let header = lines.iter().position(|line| *line == "error[E0308]: mismatched types").unwrap_or_else(|| panic!("no E0308 header:\n{text}"));
    assert_eq!(lines[header + 1], format!(" --> {}:3:5", Path::new("crates").join("leaf").join("src").join("lib.rs").display()), "{text}");
    assert_eq!(lines[header + 2..=header + 6], ["     1 | pub fn answer() -> u32 {", "     2 |     let scale = 6;", ">    3 |     \"forty-two\"", "      |     ^^^^^^^^^^^ expected `u32`, found `&str`", "     4 | }"][..], "{text}");
    assert!(!lines.contains(&"3 |     \"forty-two\""), "rustc's own snippet row survived:\n{text}");
    assert!(!text.contains("source not found"), "{text}");
}

#[proxima::test(timeout = "310s")]
#[case::plain("plain")]
#[case::agent("agent")]
#[case::rich("rich")]
async fn an_error_in_both_the_lib_and_the_lib_test_target_prints_one_diagnostic_and_counts_one_error(#[case] mode: &str) {
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(manifest())
        .args(["--tests", "--output", mode])
        .env_remove("CI")
        .env_remove("CARGO_VAL_COLOR")
        .env("NO_COLOR", "1")
        .env("CARGO_TARGET_DIR", target_directory().join(format!("build-diagnostics-dedupe-{mode}")));

    let output = fixture_output(&mut command).await;

    let (stdout, stderr) = (String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.status.code(), Some(3), "{stderr}");
    if mode == "agent" {
        assert_eq!(stdout.matches("ERROR    ").count(), 1, "the lib and lib-test targets share one error block:\n{stdout}");
        assert!(!stderr.contains("E0308"), "{stderr}");
    } else {
        assert_eq!(stderr.matches("error[E0308]: mismatched types").count(), 1, "the lib and lib-test targets share one diagnostic:\n{stderr}");
        assert_eq!(stderr.matches("warning: unused doc comment").count(), 1, "the shared warning prints once too:\n{stderr}");
    }
    assert!(mode == "agent" || stdout.contains("BUILD    FAILED  errors=1  "), "the error counts once:\n{stdout}");
}
