use std::env;
use std::fs;
use std::path::Path;
use std::process;

const SPIN_LIMIT: u64 = 40_000_000;

fn live_count(directory: &Path) -> usize {
    fs::read_dir(directory)
        .map(|entries| {
            entries
                .flatten()
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("live-"))
                .count()
        })
        .unwrap_or(0)
}

fn overlap_probe(directory: &Path, name: &str) {
    let marker = directory.join(format!("live-{}", process::id()));
    fs::write(&marker, name).expect("probe directory accepts a live marker");
    let mut observed = live_count(directory);
    let mut spins = 0_u64;
    while observed < 2 && spins < SPIN_LIMIT {
        spins += 1;
        std::hint::black_box(spins);
        if spins % 1_000_000 == 0 {
            observed = live_count(directory);
        }
    }
    observed = observed.max(live_count(directory));
    fs::write(directory.join(format!("seen-{}", process::id())), format!("{observed} {name}"))
        .expect("probe directory accepts an observation");
    fs::remove_file(&marker).expect("live marker is still present");
}

fn case(name: &str) {
    if let Some(directory) = env::var_os("CARGO_VAL_PROBE_DIR") {
        overlap_probe(Path::new(&directory), name);
    }
    let failing = env::var("CARGO_VAL_FIXTURE_FAILING").unwrap_or_default();
    assert!(
        !failing.split(',').any(|failing_name| failing_name == name),
        "{name} was selected to fail by CARGO_VAL_FIXTURE_FAILING={failing}"
    );
}


#[test]
fn case_01() {
    case("case_01");
}

#[test]
fn case_02() {
    case("case_02");
}

#[test]
fn case_03() {
    case("case_03");
}

#[test]
fn case_04() {
    case("case_04");
}

#[test]
fn case_05() {
    case("case_05");
}

#[test]
fn case_06() {
    case("case_06");
}

#[test]
fn case_07() {
    case("case_07");
}

#[test]
fn case_08() {
    case("case_08");
}

#[test]
fn case_09() {
    case("case_09");
}

#[test]
fn case_10() {
    case("case_10");
}

#[test]
fn case_11() {
    case("case_11");
}

#[test]
fn case_12() {
    case("case_12");
}

#[test]
fn case_13() {
    case("case_13");
}

#[test]
fn case_14() {
    case("case_14");
}

#[test]
fn case_15() {
    case("case_15");
}

#[test]
fn case_16() {
    case("case_16");
}

#[test]
fn case_17() {
    case("case_17");
}

#[cfg(feature = "extra")]
#[test]
fn case_18() {
    case("case_18");
}

#[cfg(feature = "extra")]
#[test]
fn case_19() {
    case("case_19");
}

#[cfg(feature = "extra")]
#[test]
fn case_20() {
    case("case_20");
}
