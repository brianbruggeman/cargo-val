use std::path::PathBuf;
use std::process::Output;

mod common;
#[path = "common/fixture.rs"]
mod fixture;
#[path = "common/binary.rs"]
mod binary;

use fixture::fixture_output;

const ESCAPE: u8 = 0x1b;

type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a [&'a str]);

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name).join("Cargo.toml")
}

async fn run(mode: &str, fixture_name: &str, environment: &[(&str, &str)], arguments: &[&str]) -> Output {
    let mut command = binary::command();
    command
        .arg("--manifest-path")
        .arg(fixture(fixture_name))
        .args(["--tests", "--no-fail-fast"])
        .args(["--output", mode])
        .args(arguments)
        .env_remove("CI")
        .env_remove("GITHUB_ACTIONS")
        .env_remove("NO_COLOR")
        .env_remove("TERM")
        .env_remove("CLAUDECODE")
        .env_remove("CARGO_VAL_OUTPUT")
        .env_remove("CARGO_VAL_COLOR")
        .env_remove("CARGO_VAL_PROCESS")
        .env_remove("RUST_TEST_THREADS")
        .env("CARGO_TARGET_DIR", PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("json-fixture-target"))
        .envs(environment.iter().copied());
    fixture_output(&mut command).await
}

fn lines_of(output: &Output) -> Vec<String> {
    let text = String::from_utf8(output.stdout.clone()).expect("json output is UTF-8");
    text.lines().map(str::to_owned).collect()
}

fn count(lines: &[String], tag: &str) -> usize {
    let needle = format!("\"t\":\"{tag}\"");
    lines.iter().filter(|line| line.starts_with("{\"v\":1,") && line[7..].starts_with(&needle)).count()
}

struct Cursor<'text> {
    bytes: &'text [u8],
    position: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.position).copied()
    }

    fn expect(&mut self, wanted: u8) -> Result<(), String> {
        match self.peek() {
            Some(found) if found == wanted => {
                self.position += 1;
                Ok(())
            }
            found => Err(format!("wanted {:?} at {} found {found:?}", wanted as char, self.position)),
        }
    }

    fn literal(&mut self, word: &[u8]) -> Result<(), String> {
        if self.bytes[self.position..].starts_with(word) {
            self.position += word.len();
            return Ok(());
        }
        Err(format!("bad literal at {}", self.position))
    }

    fn string(&mut self) -> Result<(), String> {
        self.expect(b'"')?;
        loop {
            match self.peek().ok_or("unterminated string")? {
                b'"' => return self.expect(b'"'),
                b'\\' => self.escape()?,
                control if control < 0x20 => return Err(format!("raw control byte {control:#x} at {}", self.position)),
                _ => self.position += 1,
            }
        }
    }

    fn escape(&mut self) -> Result<(), String> {
        self.position += 1;
        match self.peek().ok_or("dangling backslash")? {
            b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => self.position += 1,
            b'u' => {
                let digits = self.bytes.get(self.position + 1..self.position + 5).ok_or("short unicode escape")?;
                if !digits.iter().all(u8::is_ascii_hexdigit) {
                    return Err(format!("bad unicode escape at {}", self.position));
                }
                self.position += 5;
            }
            other => return Err(format!("bad escape {:?} at {}", other as char, self.position)),
        }
        Ok(())
    }

    fn number(&mut self) -> Result<(), String> {
        let start = self.position;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.position += 1;
        }
        if self.position == start { Err(format!("bad number at {start}")) } else { Ok(()) }
    }

    fn sequence(&mut self, close: u8, member: fn(&mut Self) -> Result<(), String>) -> Result<(), String> {
        self.position += 1;
        if self.peek() == Some(close) {
            return self.expect(close);
        }
        loop {
            member(self)?;
            if self.peek() == Some(b',') {
                self.position += 1;
            } else {
                return self.expect(close);
            }
        }
    }

    fn pair(&mut self) -> Result<(), String> {
        self.string()?;
        self.expect(b':')?;
        self.value()
    }

    fn value(&mut self) -> Result<(), String> {
        match self.peek().ok_or("missing value")? {
            b'{' => self.sequence(b'}', Self::pair),
            b'[' => self.sequence(b']', Self::value),
            b'"' => self.string(),
            b't' => self.literal(b"true"),
            b'f' => self.literal(b"false"),
            b'n' => self.literal(b"null"),
            _ => self.number(),
        }
    }
}

fn validate_object(line: &str) -> Result<(), String> {
    let mut cursor = Cursor { bytes: line.as_bytes(), position: 0 };
    if cursor.peek() != Some(b'{') {
        return Err("record is not an object".to_owned());
    }
    cursor.value()?;
    if cursor.position == line.len() { Ok(()) } else { Err(format!("trailing bytes at {}", cursor.position)) }
}

fn assert_all_objects(lines: &[String]) {
    assert!(!lines.is_empty(), "no records at all");
    for line in lines {
        validate_object(line).unwrap_or_else(|reason| panic!("{reason}: {line}"));
        assert!(line.starts_with("{\"v\":1,\"t\":\""), "version and tag come first: {line}");
    }
}

fn assert_no_escape(output: &Output) {
    assert!(!output.stdout.contains(&ESCAPE), "stdout holds an ESC byte");
    assert!(!output.stderr.contains(&ESCAPE), "stderr holds an ESC byte");
}

#[test]
fn the_validator_rejects_what_json_rejects() {
    for bad in ["{\"a\":\"x\ny\"}", "{\"a\":1,}", "{\"a\":\"\\q\"}", "{\"a\":1} x", "[1]", "{\"a\":\"\\u12g4\"}", "{\"a\":"] {
        assert!(validate_object(bad).is_err(), "accepted {bad:?}");
    }
    assert!(validate_object("{\"a\":[{\"b\":null},true,12],\"c\":\"\\u000a\\\"\"}").is_ok());
}

#[proxima::test(timeout = "310s")]
async fn a_green_run_streams_one_record_per_event_and_closes_with_the_exit_code() {
    let output = run("json", "cli_run", &[], &[]).await;
    let lines = lines_of(&output);

    assert_eq!(output.status.code(), Some(0), "{lines:?}");
    assert_all_objects(&lines);
    assert!(lines[0].starts_with("{\"v\":1,\"t\":\"run\",\"tool\":\"cargo-val\""), "{}", lines[0]);
    assert!(lines[0].contains("\"mode\":\"json\",\"mode_why\":\"flag\""), "{}", lines[0]);
    assert_eq!([count(&lines, "run"), count(&lines, "start"), count(&lines, "end")], [1, 2, 3], "{lines:?}");
    assert_eq!([count(&lines, "summary"), count(&lines, "fail"), count(&lines, "exit")], [1, 0, 1], "{lines:?}");
    assert!(lines.iter().any(|line| line.contains("\"t\":\"summary\",\"discovered\":3,\"run\":2")), "{lines:?}");
    assert_eq!(lines.last().expect("records"), "{\"v\":1,\"t\":\"exit\",\"code\":0,\"outcome\":\"green\"}");
    assert_no_escape(&output);
}

#[proxima::test(timeout = "310s")]
async fn a_failure_is_a_fail_record_with_agent_fields_then_output_then_rerun_then_exit_one() {
    let output = run("json", "cli_run", &[("CARGO_VAL_C52D_FAIL", "1")], &[]).await;
    let lines = lines_of(&output);

    assert_eq!(output.status.code(), Some(1), "{lines:?}");
    assert_all_objects(&lines);
    assert_eq!([count(&lines, "fail"), count(&lines, "output"), count(&lines, "rerun"), count(&lines, "trunc")], [1, 1, 1, 0], "{lines:?}");
    let fail = lines.iter().find(|line| line.contains("\"t\":\"fail\"")).expect("a fail record");
    assert!(fail.contains("\"id\":\"cli_run::fixture::fails_when_requested\""), "{fail}");
    assert!(fail.contains("\"class\":\"PANIC\""), "{fail}");
    assert!(fail.contains("\"file\":\"src/lib.rs\",\"line\":12,\"col\":13"), "{fail}");
    assert!(fail.contains("\"message\":\"requested fixture failure\""), "{fail}");
    assert!(fail.contains("\"rerun\":\"cargo val --no-fail-fast -E '(binary(=cli_run) & test(=fixture::fails_when_requested))'\""), "{fail}");
    assert!(lines.iter().any(|line| line.contains("\"t\":\"end\"") && line.contains("\"outcome\":\"failed\"")), "{lines:?}");
    assert_eq!(lines.last().expect("records"), "{\"v\":1,\"t\":\"exit\",\"code\":1,\"outcome\":\"failed\"}");
    assert_no_escape(&output);
}

#[proxima::test(timeout = "310s")]
async fn long_output_is_bounded_and_the_cut_is_a_trunc_record() {
    let output = run("json", "cli_run", &[("CARGO_VAL_C52D_FAIL", "1"), ("CARGO_VAL_C52D_LONG_OUTPUT", "1")], &[]).await;
    let lines = lines_of(&output);

    assert_all_objects(&lines);
    assert_eq!(count(&lines, "trunc"), 1, "{lines:?}");
    let trunc = lines.iter().find(|line| line.contains("\"t\":\"trunc\"")).expect("a trunc record");
    assert!(trunc.contains("\"what\":\"output\"") && trunc.contains("\"recover\":\"cargo test -- --exact --nocapture\""), "{trunc}");
}

#[proxima::test(timeout = "310s")]
async fn real_panic_text_with_quotes_backslashes_and_a_newline_is_escaped_and_stays_one_line() {
    let output = run("json", "cli_run", &[("CARGO_VAL_C52D_ESCAPES", "1")], &[]).await;
    let lines = lines_of(&output);

    assert_eq!(output.status.code(), Some(1), "{lines:?}");
    assert_all_objects(&lines);
    let fail = lines.iter().find(|line| line.contains("\"t\":\"fail\"")).expect("a fail record");
    assert!(fail.contains(r#""message":"path \"C:\\tmp\\out\" is missing\u000asecond line""#), "{fail}");
    let text = lines.iter().find(|line| line.contains("\"t\":\"output\"")).expect("an output record");
    assert!(text.contains(r#"path \"C:\\tmp\\out\" is missing\u000asecond line"#), "{text}");
}

#[proxima::test(timeout = "310s")]
async fn a_build_error_is_a_build_record_then_exit_three() {
    let output = run("json", "build_error", &[], &[]).await;
    let lines = lines_of(&output);

    assert_eq!(output.status.code(), Some(3), "{lines:?}");
    assert_all_objects(&lines);
    assert_eq!([count(&lines, "run"), count(&lines, "build"), count(&lines, "exit")], [1, 1, 1], "{lines:?}");
    assert_eq!(lines.last().expect("records"), "{\"v\":1,\"t\":\"exit\",\"code\":3,\"outcome\":\"build_failed\"}");
    assert_no_escape(&output);
}

#[proxima::test(timeout = "310s")]
async fn exit_codes_match_the_agent_mode_on_every_ending() {
    let cases: [Case; 4] = [
        ("cli_run", &[], &[]),
        ("cli_run", &[("CARGO_VAL_C52D_FAIL", "1")], &[]),
        ("cli_run", &[], &["-E", "test(=nope)"]),
        ("build_error", &[], &[]),
    ];
    let mut compared = 0;
    for (fixture_name, environment, arguments) in cases {
        let agent = run("agent", fixture_name, environment, arguments).await;
        let json = run("json", fixture_name, environment, arguments).await;
        assert_eq!(json.status.code(), agent.status.code(), "{fixture_name} {environment:?} {arguments:?}");
        assert_all_objects(&lines_of(&json));
        compared += 1;
    }
    assert_eq!(compared, 4);
}

#[proxima::test(timeout = "310s")]
async fn zero_tests_close_with_run_zero_and_exit_four() {
    let output = run("json", "cli_run", &[], &["-E", "test(=nope)"]).await;
    let lines = lines_of(&output);

    assert_eq!(output.status.code(), Some(4), "{lines:?}");
    assert_all_objects(&lines);
    assert!(lines.iter().any(|line| line.contains("\"t\":\"summary\"") && line.contains("\"run\":0")), "{lines:?}");
    assert_eq!(lines.last().expect("records"), "{\"v\":1,\"t\":\"exit\",\"code\":4,\"outcome\":\"no_tests\"}");
}

#[proxima::test(timeout = "310s")]
async fn a_fail_record_carries_the_bounded_unhighlighted_source_excerpt() {
    let output = run("json", "cli_run", &[("CARGO_VAL_C52D_FAIL", "1")], &[]).await;
    let lines = lines_of(&output);

    let fail = lines.iter().find(|line| line.contains("\"t\":\"fail\"")).expect("a fail record");
    let expected = concat!(
        "\"excerpt\":\"     9 |             print!(\\\"{}\\\", \\\"X\\\".repeat(5000));\\u000a",
        "    10 |         }\\u000a",
        "    11 |         if std::env::var_os(\\\"CARGO_VAL_C52D_FAIL\\\").is_some() {\\u000a",
        ">   12 |             panic!(\\\"requested fixture failure\\\");\\u000a",
        "      |             ^^^^^ panicked here\\u000a",
    );
    assert!(fail.contains(expected), "{fail}");
    assert!(fail.contains("\"excerpt_absent\":null"), "{fail}");
    assert_no_escape(&output);
}
