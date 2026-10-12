use conflaguration::codegen;
use conflaguration::{ConfigCodegen, Error, Settings, Validate, ValidationMessage};
use std::env;
use std::thread;

#[derive(Settings, ConfigCodegen)]
#[settings(prefix = "CARGO_VAL_BUILD")]
struct BuildDefaults {
    #[setting(default = 2)]
    jobs: usize,
    #[setting(default = true)]
    color: bool,
    #[setting(default = "isolated")]
    target_dir: String,
    #[setting(default = 65536)]
    spawn_string_bytes: usize,
    #[setting(default = 64)]
    spawn_argument_pointers: usize,
    #[setting(default = 1100)]
    spawn_environment_pointers: usize,
}

impl Validate for BuildDefaults {
    fn validate(&self) -> conflaguration::Result<()> {
        if self.jobs == 0 {
            return Err(Error::Validation {
                errors: vec![ValidationMessage::new("jobs", "must be greater than zero")],
            });
        }
        Ok(())
    }
}

fn main() -> conflaguration::Result<()> {
    let mut settings = conflaguration::init::<BuildDefaults>()?;
    if env::var_os("CARGO_VAL_BUILD_JOBS").is_none() && env::var_os("HOST") == env::var_os("TARGET")
    {
        settings.jobs = thread::available_parallelism().map_or(2, |count| count.get().min(64));
    }
    codegen::write_consts(&settings, "cargo_val_build.rs")?;
    codegen::rerun_for::<BuildDefaults>();
    Ok(())
}
