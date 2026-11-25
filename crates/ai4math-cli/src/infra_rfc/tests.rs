use super::*;
use ai4math_core::InfraStatus;

#[test]
fn draft_file_read_is_strict_bounded_and_never_follows_links() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let file = tmp.path().join("draft.json");
    let sample = r#"{
        "rfc_id": "RFC-01", "requested_by_claim": "Consumer", "engine":"lean4",
        "motivation":"Consumer needs a reusable definition.",
        "proposed_definitions":["Infra.bound"], "required_api_lemmas":["Infra.bound_spec"],
        "consumer_usage_sketch":"exact Infra.bound_spec",
        "smoke_test_criteria":["Compare a nontrivial case", "Prove a boundary identity"], "status":"DRAFT"
    }"#;
    fs::write(&file, sample)?;
    let decoded = read_draft(&file)?;
    assert_eq!(decoded.rfc_id, "RFC-01");
    assert_eq!(decoded.status, InfraStatus::Draft);
    assert_eq!(decoded.consumer_usage_sketch, "exact Infra.bound_spec");
    let duplicate = sample.replace(
        "\"status\":\"DRAFT\"",
        "\"status\":\"DRAFT\",\"status\":\"DRAFT\"",
    );
    for invalid in [
        duplicate,
        sample.replace("\"DRAFT\"", "\"APPROVED\""),
        format!("[{sample}]"),
        format!("{sample} null"),
    ] {
        fs::write(&file, invalid)?;
        assert!(read_draft(&file).is_err());
    }
    fs::write(&file, sample)?;
    #[cfg(unix)]
    {
        let link = tmp.path().join("draft-link.json");
        std::os::unix::fs::symlink(&file, &link)?;
        assert!(read_draft(&link).is_err());
    }
    assert!(read_draft(tmp.path()).is_err());
    fs::File::create(&file)?.set_len(INPUT_CAP + 1)?;
    assert!(read_draft(&file).is_err());
    Ok(())
}
