#[cfg(test)]
mod fixture {
    #[test]
    fn passes() {}

    #[test]
    fn fails_when_requested() { crate::pattern::write_from_env();
        if std::env::var_os("CARGO_VAL_C52D_LONG_OUTPUT").is_some() {
            print!("{}", "X".repeat(5000));
        }
        if std::env::var_os("CARGO_VAL_C52D_FAIL").is_some() {
            panic!("requested fixture failure");
        }
        if std::env::var_os("CARGO_VAL_C52D_ESCAPES").is_some() {
            panic!("path \"C:\\tmp\\out\" is missing\nsecond line");
        }
    }

    #[test]
    #[ignore]
    fn ignored_by_default() {
        panic!("ignored fixture body should not run");
    }
}

#[cfg(test)]
mod pattern;
