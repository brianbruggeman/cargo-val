pub struct Summary {
    pub count: usize,
    pub mean: f64,
    pub median: f64,
    pub min: f64,
    pub max: f64,
    pub cov: f64,
}

pub fn median(sorted: &[f64]) -> f64 {
    let count = sorted.len();
    match count {
        0 => 0.0,
        _ if count % 2 == 1 => sorted[count / 2],
        _ => f64::midpoint(sorted[count / 2 - 1], sorted[count / 2]),
    }
}

pub fn summarize(samples: &[f64]) -> Option<Summary> {
    if samples.is_empty() {
        return None;
    }
    let count = samples.len();
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let divisor = f64::from(u32::try_from(count).unwrap_or(u32::MAX));
    let mean = samples.iter().sum::<f64>() / divisor;
    let variance = samples.iter().map(|sample| (sample - mean).powi(2)).sum::<f64>() / divisor;
    let cov = if mean == 0.0 { 0.0 } else { variance.sqrt() / mean };
    Some(Summary {
        count,
        mean,
        median: median(&sorted),
        min: sorted[0],
        max: sorted[count - 1],
        cov,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_of_odd_count_is_middle_element() {
        assert!((median(&[1.0, 5.0, 9.0]) - 5.0).abs() < f64::EPSILON);
    }

    #[test]
    fn median_of_even_count_is_midpoint_of_middle_pair() {
        assert!((median(&[1.0, 2.0, 4.0, 10.0]) - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn summarize_matches_python_pstdev_on_workload_four_scale_samples() {
        let samples = [45_700_500.0, 58_563_500.0, 70_280_000.0, 70_779_000.0];
        let summary = summarize(&samples).expect("non-empty samples");
        let mean = 61_330_750.0;
        let variance: f64 = samples.iter().map(|value| (value - mean).powi(2)).sum::<f64>() / 4.0;
        assert_eq!(summary.count, 4);
        assert!((summary.mean - mean).abs() < 1e-6);
        assert!((summary.cov - variance.sqrt() / mean).abs() < 1e-12);
        assert!((summary.median - 64_421_750.0).abs() < 1e-6);
        assert!((summary.min - 45_700_500.0).abs() < f64::EPSILON);
        assert!((summary.max - 70_779_000.0).abs() < f64::EPSILON);
    }

    #[test]
    fn constant_samples_have_zero_cov() {
        let summary = summarize(&[7.0, 7.0, 7.0]).expect("non-empty samples");
        assert!(summary.cov.abs() < f64::EPSILON);
    }

    #[test]
    fn empty_samples_have_no_summary() {
        assert!(summarize(&[]).is_none());
    }
}
