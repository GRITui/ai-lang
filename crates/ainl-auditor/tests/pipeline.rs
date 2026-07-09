use ainl_auditor::{Auditor, MockBackend, Role};

#[test]
fn runs_all_five_stages_in_order() {
    let backend = MockBackend;
    let report = Auditor::new(&backend).run("sum a list of numbers").unwrap();
    assert_eq!(report.stages.len(), 5);
    let order: Vec<Role> = report.stages.iter().map(|s| s.role).collect();
    assert_eq!(order, Role::PIPELINE.to_vec());
}

#[test]
fn auditor_output_is_valid_ainl() {
    let backend = MockBackend;
    let report = Auditor::new(&backend).run("greet the user by name").unwrap();
    assert!(report.ainl_valid, "auditor AINL failed to parse: {:?}", report.ainl_error);
    // and it really parses with the core parser
    assert!(ainl_core::parse(&report.ainl).is_ok());
}

#[test]
fn each_stage_uses_its_assigned_model() {
    let backend = MockBackend;
    let report = Auditor::new(&backend).run("do a thing").unwrap();
    let auditor_stage = report.stages.iter().find(|s| s.role == Role::Auditor).unwrap();
    assert_eq!(auditor_stage.model, "phi-4-mini:3.8b");
    let orch = report.stages.iter().find(|s| s.role == Role::Orchestrator).unwrap();
    assert_eq!(orch.model, "qwen3.5:9b");
}

#[test]
fn context_prompt_has_sections_and_embeds_ainl() {
    let backend = MockBackend;
    let report = Auditor::new(&backend).run("reverse a string").unwrap();
    assert!(report.context_prompt.contains("# Audited request"));
    assert!(report.context_prompt.contains("## Plan"));
    assert!(report.context_prompt.contains("## AINL schema"));
    assert!(report.context_prompt.contains("```ainl"));
    assert!(report.context_prompt.contains("grammar-valid"));
}

#[test]
fn backend_is_described() {
    let backend = MockBackend;
    let report = Auditor::new(&backend).run("x").unwrap();
    assert!(report.backend.contains("mock"));
}
