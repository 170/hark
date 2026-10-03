mod common;
use hark::feedback::Feedback;
use std::{collections::HashSet, sync::Arc};

fn words() -> HashSet<String> {
    HashSet::from(["hello".into()])
}

#[test]
fn report_rejects_the_false_match_preserves_a_positive_and_survives_restart() {
    let directory = tempfile::tempdir().unwrap();
    let feedback = Feedback::new(Some(directory.path().into())).unwrap();
    let model = Arc::new(common::model("hello"));
    feedback.register(model.clone()).unwrap();
    let false_match = vec![[0.2; 13]; 15];
    let positive = vec![[0.11; 13]; 15];
    let original = model.evaluate(&false_match);
    assert!(original.detected);
    let id = feedback.record("hello", original.distance.unwrap());
    let reply = feedback.apply(&id, &words()).unwrap();
    assert_eq!(reply.status, "applied");
    assert!(
        !feedback
            .adjust("hello", model.evaluate(&false_match))
            .detected
    );
    assert!(feedback.adjust("hello", model.evaluate(&positive)).detected);
    assert_eq!(
        feedback.apply(&id, &words()).unwrap().threshold,
        reply.threshold
    );

    let restored = Feedback::new(Some(directory.path().into())).unwrap();
    restored.register(model.clone()).unwrap();
    assert!(
        !restored
            .adjust("hello", model.evaluate(&false_match))
            .detected
    );
    assert!(restored.apply(&id, &words()).is_err()); // Event IDs expire on restart.
    let new_id = restored.record("hello", 0.07);
    assert_ne!(new_id, id);

    // The same word with a different baseline must not inherit old feedback.
    let mut changed = (*model).clone();
    changed.templates[0][0][0] += 0.01;
    let restored = Feedback::new(Some(directory.path().into())).unwrap();
    restored.register(Arc::new(changed.clone())).unwrap();
    assert_eq!(
        restored
            .adjust("hello", changed.evaluate(&positive))
            .threshold,
        changed.threshold
    );
}

#[test]
fn overlap_unknown_ids_and_other_words_never_reduce_threshold() {
    let feedback = Feedback::new(None).unwrap();
    let mut model = common::model("hello");
    model.calibration.positive_max_distance = 0.075;
    feedback.register(Arc::new(model.clone())).unwrap();
    let id = feedback.record("hello", 0.08);
    assert!(feedback.apply("missing", &words()).is_err());
    assert!(
        feedback
            .apply(&id, &HashSet::from(["other".into()]))
            .is_err()
    );
    let reply = feedback.apply(&id, &words()).unwrap();
    assert_eq!(reply.status, "needs_examples");
    assert_eq!(reply.threshold, model.threshold);
    assert_eq!(
        feedback
            .adjust("hello", model.evaluate(&model.templates[0]))
            .threshold,
        model.threshold
    );
}

#[test]
fn failed_save_does_not_change_detector_and_report_can_be_retried() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("feedback");
    let feedback = Feedback::new(Some(path.clone())).unwrap();
    let model = Arc::new(common::model("hello"));
    feedback.register(model.clone()).unwrap();
    let id = feedback.record("hello", 0.08);
    std::fs::write(&path, b"not a directory").unwrap();
    assert!(feedback.apply(&id, &words()).is_err());
    assert_eq!(
        feedback
            .adjust("hello", model.evaluate(&model.templates[0]))
            .threshold,
        0.1
    );
    std::fs::remove_file(path).unwrap();
    assert_eq!(feedback.apply(&id, &words()).unwrap().status, "applied");
}

#[test]
fn recent_history_is_bounded_and_older_reports_are_rejected() {
    let feedback = Feedback::new(None).unwrap();
    feedback.register(Arc::new(common::model("hello"))).unwrap();
    let oldest = feedback.record("hello", 0.08);
    let mut latest = String::new();
    for _ in 0..256 {
        latest = feedback.record("hello", 0.08);
    }
    assert!(feedback.apply(&oldest, &words()).is_err());
    assert_eq!(feedback.apply(&latest, &words()).unwrap().status, "applied");
}

#[test]
fn positive_feedback_protects_real_matches_and_survives_restart() {
    use hark::feedback::Label;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(common::model("hello"));
    let feedback = Feedback::new(Some(directory.path().into())).unwrap();
    feedback.register(model.clone()).unwrap();
    let positive = feedback.record("hello", 0.08);
    let reply = feedback
        .apply_label(&positive, Label::TruePositive, &words())
        .unwrap();
    assert_eq!(reply.status, "recorded");
    assert_eq!(reply.threshold, model.threshold);
    assert_eq!(
        feedback
            .apply_label(&positive, Label::TruePositive, &words())
            .unwrap()
            .status,
        "recorded"
    );
    assert!(feedback.apply(&positive, &words()).is_err());

    let feedback = Feedback::new(Some(directory.path().into())).unwrap();
    feedback.register(model.clone()).unwrap();
    let overlapping = feedback.record("hello", 0.07);
    assert_eq!(
        feedback.apply(&overlapping, &words()).unwrap().status,
        "needs_examples"
    );
    let negative = feedback.record("hello", 0.09);
    let reply = feedback.apply(&negative, &words()).unwrap();
    assert_eq!(reply.status, "applied");
    assert!(reply.threshold >= 0.08 && reply.threshold < 0.09);
    // A delayed positive must not undo a stricter threshold already installed.
    let late = feedback.record("hello", 0.095);
    assert_eq!(
        feedback
            .apply_label(&late, Label::TruePositive, &words())
            .unwrap()
            .status,
        "needs_examples"
    );
}

#[test]
fn reset_restores_baseline_for_one_word_and_invalidates_its_old_event_ids() {
    use hark::feedback::Label;
    let directory = tempfile::tempdir().unwrap();
    let feedback = Feedback::new(Some(directory.path().into())).unwrap();
    let model = Arc::new(common::model("hello"));
    feedback.register(model.clone()).unwrap();
    feedback.register(Arc::new(common::model("other"))).unwrap();
    let all = HashSet::from(["hello".into(), "other".into()]);
    let positive = feedback.record("hello", 0.07);
    feedback
        .apply_label(&positive, Label::TruePositive, &all)
        .unwrap();
    let negative = feedback.record("hello", 0.09);
    feedback.apply(&negative, &all).unwrap();
    let other = feedback.record("other", 0.08);
    let other_threshold = feedback.apply(&other, &all).unwrap().threshold;
    assert!(
        feedback
            .reset("hello", &HashSet::from(["other".into()]))
            .is_err()
    );
    let reply = feedback.reset("hello", &words()).unwrap();
    assert_eq!(reply.event, "reset_result");
    assert_eq!(reply.threshold, model.threshold);
    assert!(feedback.apply(&positive, &all).is_err());
    assert!(feedback.apply(&negative, &all).is_err());
    assert_eq!(
        feedback.apply(&other, &all).unwrap().threshold,
        other_threshold
    );
    assert_eq!(
        feedback.reset("hello", &words()).unwrap().threshold,
        model.threshold
    );
    let restored = Feedback::new(Some(directory.path().into())).unwrap();
    restored.register(model.clone()).unwrap();
    assert_eq!(
        restored
            .adjust("hello", model.evaluate(&model.templates[0]))
            .threshold,
        model.threshold
    );
    // The reset removed the confirmed-positive bound too.
    let negative = restored.record("hello", 0.06);
    assert_eq!(
        restored.apply(&negative, &words()).unwrap().status,
        "applied"
    );
}

#[test]
fn positive_save_and_reset_failures_leave_live_state_unchanged() {
    use hark::feedback::Label;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("feedback");
    let feedback = Feedback::new(Some(root.clone())).unwrap();
    let model = Arc::new(common::model("hello"));
    feedback.register(model.clone()).unwrap();
    std::fs::write(&root, b"not a directory").unwrap();
    let positive = feedback.record("hello", 0.08);
    assert!(
        feedback
            .apply_label(&positive, Label::TruePositive, &words())
            .is_err()
    );
    std::fs::remove_file(&root).unwrap();
    let negative = feedback.record("hello", 0.07);
    let tuned = feedback.apply(&negative, &words()).unwrap();
    assert_eq!(tuned.status, "applied");
    // Replace the generated feedback directory with a regular file to force a reset failure.
    let backup = directory.path().join("backup");
    std::fs::rename(&root, &backup).unwrap();
    std::fs::write(&root, b"not a directory").unwrap();
    assert!(feedback.reset("hello", &words()).is_err());
    assert_eq!(
        feedback
            .adjust("hello", model.evaluate(&model.templates[0]))
            .threshold,
        tuned.threshold
    );
    assert!(feedback.apply(&negative, &words()).is_ok());
    std::fs::remove_file(&root).unwrap();
    std::fs::rename(backup, &root).unwrap();
    assert_eq!(
        feedback.reset("hello", &words()).unwrap().threshold,
        model.threshold
    );
}

#[test]
fn legacy_feedback_file_is_loaded_and_migrates_on_positive_report() {
    use hark::feedback::Label;
    let directory = tempfile::tempdir().unwrap();
    let model = Arc::new(common::model("hello"));
    let path = directory.path().join("v1/68656c6c6f/threshold.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "version":1,"base": &*model,"threshold":0.06
        }))
        .unwrap(),
    )
    .unwrap();
    let feedback = Feedback::new(Some(directory.path().into())).unwrap();
    feedback.register(model.clone()).unwrap();
    assert_eq!(
        feedback
            .adjust("hello", model.evaluate(&model.templates[0]))
            .threshold,
        0.06
    );
    let id = feedback.record("hello", 0.05);
    feedback
        .apply_label(&id, Label::TruePositive, &words())
        .unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(saved["version"], 2);
    let restored = Feedback::new(Some(directory.path().into())).unwrap();
    restored.register(model).unwrap();
    let negative = restored.record("hello", 0.04);
    assert_eq!(
        restored.apply(&negative, &words()).unwrap().status,
        "needs_examples"
    );
}
