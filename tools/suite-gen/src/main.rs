use std::process::ExitCode;

use suite_gen::config::parse;
use suite_gen::run;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = parse(&args).and_then(|config| run(&config).map(|result| (config, result)));
    match outcome {
        Ok((config, (plan, calibration))) => {
            println!(
                "wrote {} tests to {} planned_serial_ms={:.3} effective_median_ms={:.4} iters_per_ms={:.1} ({}, {} generator)",
                plan.tests.len(),
                config.out.display(),
                plan.total_ms,
                plan.effective_median_ms,
                calibration.iters_per_ms,
                calibration.origin,
                calibration.generator_profile
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("suite-gen: {error}");
            ExitCode::FAILURE
        }
    }
}
