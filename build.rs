use conflaguration::codegen;
use conflaguration::{ConfigCodegen, Error, Settings, Validate, ValidationMessage};
use std::env;
use std::thread;

const MAX_JOBS: usize = 64;

#[derive(Settings, ConfigCodegen)]
#[settings(prefix = "CARGO_VAL_BUILD")]
struct BuildDefaults {
    #[setting(default = 2)]
    jobs: usize,
    #[setting(default = "1x")]
    shard_jobs: String,
    #[setting(default = true)]
    color: bool,
    #[setting(default = "shared")]
    target_dir: String,
    #[setting(default = "auto")]
    process: String,
    #[setting(default = "auto")]
    output: String,
    #[setting(default = "off")]
    exit_with_parent: String,
    #[setting(default = 4096)]
    shard_names: usize,
    #[setting(default = 64)]
    shard_retry_slots: usize,
    #[setting(default = 32767)]
    shard_command_units: usize,
    #[setting(default = 8_388_608)]
    shard_arena_bytes: usize,
    #[setting(default = 5_373_952)]
    sched_record_bytes: usize,
    #[setting(default = 262_144)]
    spawn_string_bytes: usize,
    #[setting(default = 4160)]
    spawn_argument_pointers: usize,
    #[setting(default = 1100)]
    spawn_environment_pointers: usize,
    #[setting(default = 65536)]
    status_queue_bytes: usize,
    #[setting(default = 72)]
    signal_slots: usize,
}

impl Validate for BuildDefaults {
    fn validate(&self) -> conflaguration::Result<()> {
        let mut errors = Vec::new();
        if self.jobs == 0 {
            errors.push(ValidationMessage::new("jobs", "must be greater than zero"));
        }
        if self.shard_jobs.is_empty() || !self.shard_jobs.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'x')) {
            errors.push(ValidationMessage::new("shard_jobs", "must be a count such as 8 or a multiplier of the core count such as 0.5x, 1x, 2x"));
        }
        if !matches!(self.process.as_str(), "auto" | "shard" | "test") {
            errors.push(ValidationMessage::new("process", "must be auto, shard, or test"));
        }
        if !matches!(self.output.as_str(), "auto" | "rich" | "plain") {
            errors.push(ValidationMessage::new("output", "must be auto, rich, or plain"));
        }
        if !matches!(self.exit_with_parent.as_str(), "on" | "off") {
            errors.push(ValidationMessage::new("exit_with_parent", "must be on or off"));
        }
        if self.shard_retry_slots < MAX_JOBS {
            errors.push(ValidationMessage::new(
                "shard_retry_slots",
                "must be at least 64, the most jobs the run accepts (jobs above 64 is rejected)",
            ));
        }
        if self.signal_slots < MAX_JOBS + 1 {
            errors.push(ValidationMessage::new(
                "signal_slots",
                "must be at least 65, one slot for each of the 64 jobs the run accepts plus the doc test group",
            ));
        }
        if self.shard_names == 0 || self.shard_names + 64 > self.spawn_argument_pointers {
            errors.push(ValidationMessage::new(
                "shard_names",
                "must be nonzero and leave 64 argument pointers for the program and flags",
            ));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(Error::Validation { errors })
        }
    }
}

#[derive(Settings, ConfigCodegen)]
#[settings(prefix = "CARGO_VAL_VIEW")]
struct ViewDefaults {
    #[setting(default = 64)]
    failures: usize,
    #[setting(default = 65536)]
    name_bytes: usize,
    #[setting(default = 8192)]
    rerun_bytes: usize,
    #[setting(default = 16384)]
    frame_bytes: usize,
}

impl Validate for ViewDefaults {
    fn validate(&self) -> conflaguration::Result<()> {
        let mut errors = Vec::new();
        if self.failures == 0 {
            errors.push(ValidationMessage::new("failures", "must be greater than zero"));
        }
        if self.name_bytes == 0 {
            errors.push(ValidationMessage::new("name_bytes", "must be greater than zero"));
        }
        if self.frame_bytes < self.rerun_bytes + 4096 {
            errors.push(ValidationMessage::new("frame_bytes", "must hold the rerun command plus a line, at least rerun_bytes + 4096"));
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(Error::Validation { errors })
        }
    }
}

fn main() -> conflaguration::Result<()> {
    let mut settings = conflaguration::init::<BuildDefaults>()?;
    if env::var_os("CARGO_VAL_BUILD_JOBS").is_none() && env::var_os("HOST") == env::var_os("TARGET")
    {
        settings.jobs = thread::available_parallelism().map_or(2, |count| count.get().min(MAX_JOBS));
    }
    codegen::write_consts(&settings, "cargo_val_build.rs")?;
    codegen::rerun_for::<BuildDefaults>();
    let view = conflaguration::init::<ViewDefaults>()?;
    codegen::write_consts(&view, "cargo_val_view.rs")?;
    codegen::rerun_for::<ViewDefaults>();
    Ok(())
}
