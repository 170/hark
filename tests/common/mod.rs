use hark::engine::{Calibration, Model};

pub fn model(word: &str) -> Model {
    Model {
        version: 1,
        word: word.into(),
        threshold: 0.1,
        templates: vec![vec![[0.1; 13]; 15]; 3],
        calibration: Calibration {
            method: "test".into(),
            positive_max_distance: 0.0,
            negative_min_distance: None,
            negative_count: 0,
        },
    }
}
